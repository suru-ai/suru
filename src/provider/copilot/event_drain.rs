//! Orders Copilot's final on-wire Session events ahead of a harness crash.
//!
//! The SDK reads stdout on its own task and routes each `session.event` through two more tasks
//! before a [`github_copilot_sdk::EventSubscription`] sees it. The process supervisor can observe
//! the child exit while those tasks still hold complete frames that were written before it died.
//! This module keeps a content-free ledger of the last event identifier read for each Session and
//! the last one delivered by the SDK. Once stdout ends, matching those identifiers is the barrier
//! that says every preceding event has reached Suru's own queue.

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
};

use serde::Deserialize;
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::watch,
};

/// Shared progress between the stdout observer and the Session-event drain.
#[derive(Clone)]
pub(super) struct CopilotEventDrain {
    state: Arc<StdMutex<DrainState>>,
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct DrainState {
    stdout_ended: bool,
    wire_tail: HashMap<String, String>,
    delivered_tail: HashMap<String, String>,
    /// Events read off the wire that a Session waits to see reach Suru's queue, by Session. Each
    /// leaves the set as it is delivered.
    awaited: HashMap<String, HashSet<String>>,
}

impl CopilotEventDrain {
    pub(super) fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Arc::new(StdMutex::new(DrainState::default())),
            changed,
        }
    }

    /// Observes complete JSON-RPC frames while leaving the byte stream unchanged for the SDK.
    pub(super) fn observe<R: AsyncRead + Unpin>(&self, reader: R) -> EventTrackingReader<R> {
        EventTrackingReader {
            reader,
            frames: FrameBuffer::default(),
            drain: self.clone(),
        }
    }

    /// Starts one Session's drain accounting after its SDK subscription already exists.
    pub(super) fn checkpoint(&self, session_id: impl Into<String>) -> EventDrainCheckpoint {
        let session_id = session_id.into();
        let baseline = self
            .state
            .lock()
            .expect("Copilot event-drain lock is not poisoned")
            .wire_tail
            .get(&session_id)
            .cloned();
        EventDrainCheckpoint {
            drain: self.clone(),
            session_id,
            baseline,
        }
    }

    fn record_wire_event(&self, session_id: String, event_id: String) {
        self.state
            .lock()
            .expect("Copilot event-drain lock is not poisoned")
            .wire_tail
            .insert(session_id, event_id);
        self.notify();
    }

    fn record_delivered_event(&self, session_id: String, event_id: String) {
        {
            let mut state = self
                .state
                .lock()
                .expect("Copilot event-drain lock is not poisoned");
            if let Some(awaited) = state.awaited.get_mut(&session_id) {
                awaited.remove(&event_id);
            }
            state.delivered_tail.insert(session_id, event_id);
        }
        self.notify();
    }

    fn end_stdout(&self) {
        let changed = {
            let mut state = self
                .state
                .lock()
                .expect("Copilot event-drain lock is not poisoned");
            if state.stdout_ended {
                false
            } else {
                state.stdout_ended = true;
                true
            }
        };
        if changed {
            self.notify();
        }
    }

    fn notify(&self) {
        let next = self.changed.borrow().wrapping_add(1);
        self.changed.send_replace(next);
    }
}

/// One Session's position when Suru subscribed to its SDK timeline.
#[derive(Clone)]
pub(super) struct EventDrainCheckpoint {
    drain: CopilotEventDrain,
    session_id: String,
    baseline: Option<String>,
}

impl EventDrainCheckpoint {
    /// Records an event only after it has entered Suru's unbounded projection queue.
    pub(super) fn delivered(&self, event_id: String) {
        self.drain
            .record_delivered_event(self.session_id.clone(), event_id);
    }

    /// Waits for stdout to end and for its final event after this checkpoint to reach Suru.
    pub(super) async fn wait_until_drained(&self) {
        let mut changed = self.drain.changed.subscribe();
        loop {
            let drained = {
                let state = self
                    .drain
                    .state
                    .lock()
                    .expect("Copilot event-drain lock is not poisoned");
                if !state.stdout_ended {
                    false
                } else {
                    let wire_tail = state.wire_tail.get(&self.session_id);
                    wire_tail == self.baseline.as_ref()
                        || wire_tail.is_some_and(|wire_tail| {
                            state.delivered_tail.get(&self.session_id) == Some(wire_tail)
                        })
                }
            };
            if drained {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    /// Waits until every event read off the wire for this Session so far has reached Suru's own
    /// queue, or until no more can, the wire having ended. An answer the CLI wrote after those
    /// events can then join the queue behind them, in the order the CLI wrote them, though the
    /// SDK hands answers and events to Suru apart.
    pub(super) async fn wait_until_caught_up(&self) {
        let mut changed = self.drain.changed.subscribe();
        let target = {
            let mut state = self
                .drain
                .state
                .lock()
                .expect("Copilot event-drain lock is not poisoned");
            let Some(target) = state.wire_tail.get(&self.session_id).cloned() else {
                return;
            };
            // Events read before this checkpoint may never reach its subscription, and the latest
            // one read reaching it is everything before it having reached it too.
            if Some(&target) == self.baseline.as_ref()
                || state.delivered_tail.get(&self.session_id) == Some(&target)
            {
                return;
            }
            state
                .awaited
                .entry(self.session_id.clone())
                .or_default()
                .insert(target.clone());
            target
        };
        loop {
            {
                let mut state = self
                    .drain
                    .state
                    .lock()
                    .expect("Copilot event-drain lock is not poisoned");
                let awaiting = state
                    .awaited
                    .get(&self.session_id)
                    .is_some_and(|awaited| awaited.contains(&target));
                if !awaiting || state.stdout_ended {
                    if let Some(awaited) = state.awaited.get_mut(&self.session_id) {
                        awaited.remove(&target);
                        if awaited.is_empty() {
                            state.awaited.remove(&self.session_id);
                        }
                    }
                    return;
                }
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

/// An unchanged stdout stream with a sidecar parser for event identifiers.
pub(super) struct EventTrackingReader<R> {
    reader: R,
    frames: FrameBuffer,
    drain: CopilotEventDrain,
}

impl<R: AsyncRead + Unpin> AsyncRead for EventTrackingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.reader).poll_read(context, buffer);
        match &result {
            Poll::Ready(Ok(())) => {
                let received = &buffer.filled()[filled_before..];
                if received.is_empty() {
                    self.drain.end_stdout();
                } else {
                    let drain = self.drain.clone();
                    self.frames.push(received, &drain);
                }
            }
            Poll::Ready(Err(_)) => self.drain.end_stdout(),
            Poll::Pending => {}
        }
        result
    }
}

impl<R> Drop for EventTrackingReader<R> {
    fn drop(&mut self) {
        // The SDK also drops its reader after a protocol error, without polling the pipe to EOF.
        // That is still the end of what can traverse this connection, so crash settlement must not
        // wait forever for another read.
        self.drain.end_stdout();
    }
}

#[derive(Default)]
struct FrameBuffer {
    bytes: Vec<u8>,
}

impl FrameBuffer {
    fn push(&mut self, bytes: &[u8], drain: &CopilotEventDrain) {
        self.bytes.extend_from_slice(bytes);
        loop {
            let Some((header_end, separator_len)) = header_boundary(&self.bytes) else {
                return;
            };
            let body_start = header_end + separator_len;
            let Some(content_length) = content_length(&self.bytes[..header_end]) else {
                self.bytes.drain(..body_start);
                continue;
            };
            let frame_end = body_start.saturating_add(content_length);
            if self.bytes.len() < frame_end {
                return;
            }
            if let Ok(frame) =
                serde_json::from_slice::<WireMessage>(&self.bytes[body_start..frame_end])
                && frame.method.as_deref() == Some("session.event")
                && let Some(params) = frame.params
                && let Ok(notification) =
                    serde_json::from_value::<github_copilot_sdk::SessionEventNotification>(params)
            {
                drain.record_wire_event(notification.session_id.to_string(), notification.event.id);
            }
            self.bytes.drain(..frame_end);
        }
    }
}

fn header_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf <= lf => Some((crlf, 4)),
        (Some(_), Some(lf)) => Some((lf, 2)),
        (Some(crlf), None) => Some((crlf, 4)),
        (None, Some(lf)) => Some((lf, 2)),
        (None, None) => None,
    }
}

fn content_length(header: &[u8]) -> Option<usize> {
    std::str::from_utf8(header)
        .ok()?
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))?
        .1
        .trim()
        .parse()
        .ok()
}

#[derive(Deserialize)]
struct WireMessage {
    method: Option<String>,
    params: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::{Duration, timeout},
    };

    use super::CopilotEventDrain;

    #[tokio::test]
    async fn a_crash_barrier_waits_until_the_final_on_wire_event_reaches_suru() {
        let drain = CopilotEventDrain::new();
        let checkpoint = drain.checkpoint("session-1");
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut observed = drain.observe(reader);
        let body = r#"{"jsonrpc":"2.0","method":"session.event","params":{"sessionId":"session-1","event":{"id":"last","timestamp":"2026-01-01T00:00:00Z","parentId":null,"type":"assistant.message_delta","data":{}}}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());

        writer
            .write_all(frame.as_bytes())
            .await
            .expect("write event");
        drop(writer);
        let mut received = Vec::new();
        observed
            .read_to_end(&mut received)
            .await
            .expect("SDK-side reader reaches EOF");

        assert!(
            timeout(Duration::from_millis(10), checkpoint.wait_until_drained())
                .await
                .is_err(),
            "stdout EOF alone cannot overtake an event still inside the SDK"
        );
        checkpoint.delivered("last".to_owned());
        timeout(Duration::from_millis(10), checkpoint.wait_until_drained())
            .await
            .expect("the final delivered event opens the crash barrier");
    }

    fn event_frame(id: &str) -> String {
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"session.event","params":{{"sessionId":"session-1","event":{{"id":"{id}","timestamp":"2026-01-01T00:00:00Z","parentId":null,"type":"session.compaction_complete","data":{{"success":true}}}}}}}}"#
        );
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[tokio::test]
    async fn an_answer_waits_until_every_event_read_before_it_reaches_suru() {
        let drain = CopilotEventDrain::new();
        let checkpoint = drain.checkpoint("session-1");
        timeout(Duration::from_millis(10), checkpoint.wait_until_caught_up())
            .await
            .expect("nothing read since subscribing is nothing to wait for");

        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut observed = drain.observe(reader);
        for id in ["first", "second"] {
            writer
                .write_all(event_frame(id).as_bytes())
                .await
                .expect("write event");
        }
        let mut received = vec![0; 4096];
        let mut read = 0;
        while read < event_frame("first").len() + event_frame("second").len() {
            read += observed
                .read(&mut received[read..])
                .await
                .expect("SDK-side reader reads the frames");
        }

        let caught_up = tokio::spawn({
            let checkpoint = checkpoint.clone();
            async move { checkpoint.wait_until_caught_up().await }
        });
        checkpoint.delivered("first".to_owned());
        assert!(
            timeout(Duration::from_millis(10), async {
                while !caught_up.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "an event read before the answer is still on its way"
        );
        // Something Suru never read off the wire reaching it says nothing of what did.
        checkpoint.delivered("replayed".to_owned());
        checkpoint.delivered("second".to_owned());
        timeout(Duration::from_millis(10), caught_up)
            .await
            .expect("every event read before the answer has reached Suru")
            .expect("the wait ends cleanly");
        timeout(Duration::from_millis(10), checkpoint.wait_until_caught_up())
            .await
            .expect("a Session already caught up waits on nothing");
    }
}
