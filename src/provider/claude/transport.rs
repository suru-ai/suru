//! The stream-json transport Suru speaks over a Claude Code CLI process's stdio.
//!
//! [`StreamJsonTransport::launch`] is the only way to obtain one: it spawns a supervised CLI
//! process in stream-json mode and returns the control-request channel. Control requests correlate
//! by `request_id`; every other message the CLI writes is conversation, forwarded to the
//! [`ConversationSink`] the caller launched with — a discovery launches without one and the
//! conversation is dropped unread. Every path that ends the connection fails the in-flight
//! requests exactly once, and a lost process reaches the sink as the error that took it.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, Notify, mpsc, oneshot, watch},
    time::{Duration, timeout},
};

use super::{
    claude_error,
    wire::{ControlRequest, ControlRequestEnvelope, ControlResponse},
};
use crate::provider::{
    ProviderError, concise_remote_message,
    harness::{
        HarnessLink, HarnessSpec, ProcessGuard, ProcessRegistry, ProcessStdio,
        spawn_harness_process, supervise_harness_process,
    },
};

/// Where a launched process's conversation messages go: everything the CLI writes that is not a
/// control response, the failure that ends the connection when the process is lost, and the end
/// of the process's output once it has all been delivered.
pub(super) type ConversationSink = mpsc::UnboundedSender<Result<ConversationItem, ProviderError>>;

/// One thing a launched process delivers to its Session's conversation.
#[derive(Debug)]
pub(super) enum ConversationItem {
    /// A message the CLI wrote.
    Message(Value),
    /// The process's output has ended, behind everything it wrote: whatever the process was
    /// running died with it and will never report settling. A Session outlives its processes, so
    /// this is where one process's work gives way to the next one's.
    ProcessEnded,
}

/// Which native filesystem settings a Claude process is allowed to load. User Sessions and Skill
/// discovery deliberately use Claude's personal and project sources (ADR 0013); probes and Model
/// discovery stay isolated from them.
#[derive(Clone, Copy)]
pub(super) enum ClaudeSettingSources {
    Isolated,
    PersonalAndProject,
}

impl ClaudeSettingSources {
    fn argument(self) -> &'static str {
        match self {
            Self::Isolated => "",
            Self::PersonalAndProject => "user,project",
        }
    }
}

/// The arguments that put the CLI in stream-json mode: newline-delimited JSON both ways.
const CLAUDE_STREAM_JSON_ARGS: [&str; 6] = [
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
];

/// Why a control request produced no answer.
///
/// The two are told apart because they say different things about the CLI: one Suru asked
/// something it will not serve, against one that said nothing at all — a request it ignored, or a
/// process that went down under it. Only the first is the CLI's own account of itself, which is
/// what a caller reading the CLI's capabilities — the availability probe — has to go on.
pub(super) enum ControlFailure {
    /// The CLI answered the request by refusing it.
    Refused(ProviderError),
    /// Everything else: a request never answered, a transport that ended, a process that was lost.
    Failed(ProviderError),
}

impl ControlFailure {
    /// The failure as a caller that draws no distinction reports it.
    pub(super) fn into_error(self) -> ProviderError {
        match self {
            Self::Refused(error) | Self::Failed(error) => error,
        }
    }

    /// Whether the CLI itself refused the request, rather than leaving it unanswered.
    pub(super) fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused(_))
    }
}

type PendingResponse = oneshot::Sender<Result<Value, ControlFailure>>;

struct TransportState {
    pending: StdMutex<HashMap<String, PendingResponse>>,
    /// Set once the process's output has been read to its end and delivered.
    drained: watch::Sender<bool>,
    terminated: AtomicBool,
    conversation: Option<ConversationSink>,
    decision_settlements: Arc<DecisionSettlements>,
}

#[derive(Default)]
struct DecisionSettlements {
    active: AtomicUsize,
    changed: Notify,
}

pub(super) struct DecisionSettlement(Arc<DecisionSettlements>);

impl Drop for DecisionSettlement {
    fn drop(&mut self) {
        let _ = self
            .0
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                active.checked_sub(1)
            });
        self.0.changed.notify_waiters();
    }
}

impl DecisionSettlements {
    fn acquire(self: &Arc<Self>) -> DecisionSettlement {
        self.active.fetch_add(1, Ordering::AcqRel);
        DecisionSettlement(self.clone())
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    fn release_all(&self) {
        self.active.store(0, Ordering::Release);
        self.changed.notify_waiters();
    }
}

/// Cancellation must not leave an optional query registered indefinitely.
struct PendingRequest<'a> {
    state: &'a TransportState,
    key: &'a str,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.state
            .pending
            .lock()
            .expect("Claude pending request lock is not poisoned")
            .remove(self.key);
    }
}

/// A launched connection to one supervised Claude Code CLI process.
pub(super) struct ClaudeConnection {
    pub(super) transport: StreamJsonTransport,
    pub(super) process: Arc<ProcessGuard>,
}

#[derive(Clone)]
pub(super) struct StreamJsonTransport {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<TransportState>,
    next_id: Arc<AtomicI64>,
    _process: Arc<ProcessGuard>,
}

impl StreamJsonTransport {
    /// Launches a supervised CLI process in stream-json mode with `args` appended to the mode's
    /// own, working in `cwd` when one is given. Conversation messages go to `conversation`; a
    /// caller with no interest in them — a discovery — launches with `None` and they are dropped.
    /// The CLI completes no handshake of its own; it is ready as soon as it is running.
    pub(super) async fn launch(
        executable: &OsStr,
        args: impl IntoIterator<Item = OsString>,
        cwd: Option<PathBuf>,
        conversation: Option<ConversationSink>,
        processes: ProcessRegistry,
        setting_sources: ClaudeSettingSources,
    ) -> Result<ClaudeConnection, ProviderError> {
        let spec = HarnessSpec {
            executable: executable.to_owned(),
            args: CLAUDE_STREAM_JSON_ARGS
                .iter()
                .map(OsString::from)
                .chain([
                    OsString::from("--setting-sources"),
                    OsString::from(setting_sources.argument()),
                ])
                .chain(args)
                .collect(),
            name: super::CLAUDE_HARNESS_NAME.to_owned(),
            cwd,
        };
        let (process, ProcessStdio { stdin, stdout }) = spawn_harness_process(&spec)?;
        let state = Arc::new(TransportState {
            pending: StdMutex::new(HashMap::new()),
            drained: watch::Sender::new(false),
            terminated: AtomicBool::new(false),
            conversation,
            decision_settlements: Arc::new(DecisionSettlements::default()),
        });
        let writer = Arc::new(Mutex::new(Some(stdin)));
        let link = TransportLink {
            writer: writer.clone(),
            state: state.clone(),
        };
        let (process, _exit) = supervise_harness_process(process, processes, link).await?;
        tokio::spawn(read_stdout(stdout, state.clone()));

        let transport = Self {
            writer,
            state,
            next_id: Arc::new(AtomicI64::new(1)),
            _process: process.clone(),
        };
        Ok(ClaudeConnection { transport, process })
    }

    /// Issues one control request and waits for the CLI's answer to it, up to `request_timeout`.
    pub(super) async fn control_request(
        &self,
        request: &ControlRequest,
        request_timeout: Duration,
    ) -> Result<Value, ControlFailure> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(ControlFailure::Failed(
                claude_error("Claude Code CLI transport has ended").mark_session_lost(),
            ));
        }
        let key = format!("suru-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (response_tx, response_rx) = oneshot::channel();
        self.state
            .pending
            .lock()
            .expect("Claude pending request lock is not poisoned")
            .insert(key.clone(), response_tx);
        // Remove correlation state on timeout or cancellation, including while stdin is busy.
        let _pending = PendingRequest {
            state: &self.state,
            key: &key,
        };
        match timeout(request_timeout, async {
            write_json_line(
                &self.writer,
                &ControlRequestEnvelope::new(&key, request),
                "write to Claude Code CLI",
            )
            .await
            .map_err(ControlFailure::Failed)?;
            response_rx.await.unwrap_or_else(|_| {
                Err(ControlFailure::Failed(
                    claude_error("Claude Code CLI ended before the control request completed")
                        .mark_session_lost(),
                ))
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ControlFailure::Failed(claude_error(format!(
                "Claude Code CLI timed out handling `{}`",
                request.subtype()
            )))),
        }
    }

    /// Writes one conversation message — a user message carrying a Prompt — to the CLI's stdin.
    pub(super) async fn send<T: serde::Serialize>(&self, message: &T) -> Result<(), ProviderError> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(claude_error("Claude Code CLI transport has ended").mark_session_lost());
        }
        write_json_line(&self.writer, message, "write to Claude Code CLI").await
    }

    pub(super) fn decision_settlement(&self) -> Result<DecisionSettlement, ProviderError> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(claude_error("Claude Code CLI transport has ended").mark_session_lost());
        }
        Ok(self.state.decision_settlements.acquire())
    }

    pub(super) async fn close(&self) {
        close_transport(
            &self.state,
            claude_error("Claude Code CLI transport closed during shutdown"),
        );
        close_stdin(&self.writer).await;
    }

    /// Waits until everything the process wrote has been delivered to the conversation, its end
    /// included. Only a process that has stopped is sure to get there: its output ends when the
    /// last of its process tree lets go of the pipe.
    pub(super) async fn drained(&self) {
        let mut drained = self.state.drained.subscribe();
        let _ = drained.wait_for(|drained| *drained).await;
    }
}

/// The process supervisor's handle to the transport running over the process it owns.
struct TransportLink {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<TransportState>,
}

impl HarnessLink for TransportLink {
    fn close(&self) {
        close_transport(
            &self.state,
            claude_error("Claude Code CLI transport closed during shutdown"),
        );
    }

    fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(close_stdin(&self.writer))
    }

    fn terminate(&self, error: ProviderError) {
        terminate_transport(&self.state, error);
    }
}

async fn close_stdin(writer: &Arc<Mutex<Option<ChildStdin>>>) {
    let stdin = writer.lock().await.take();
    if let Some(mut stdin) = stdin {
        let _ = stdin.shutdown().await;
    }
}

async fn write_json_line<T: serde::Serialize + ?Sized>(
    writer: &Arc<Mutex<Option<ChildStdin>>>,
    value: &T,
    operation: &str,
) -> Result<(), ProviderError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| claude_error(format!("could not encode Claude message: {error}")))?;
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    let writer = writer
        .as_mut()
        .ok_or_else(|| claude_error("Claude Code CLI transport has ended").mark_session_lost())?;
    writer.write_all(&bytes).await.map_err(|error| {
        claude_error(format!("could not {operation}: {error}")).mark_session_lost()
    })?;
    writer.flush().await.map_err(|error| {
        claude_error(format!("could not flush after {operation}: {error}")).mark_session_lost()
    })
}

async fn read_stdout(stdout: ChildStdout, state: Arc<TransportState>) {
    read_lines(stdout, &state).await;
    // However the output ended, nothing more of this process's reaches the conversation, so its
    // end rides behind everything it wrote.
    if let Some(conversation) = &state.conversation {
        let _ = conversation.send(Ok(ConversationItem::ProcessEnded));
    }
    state.drained.send_replace(true);
}

async fn read_lines(stdout: ChildStdout, state: &Arc<TransportState>) {
    let mut lines = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            // The supervisor's terminate follows the CLI's exit and closes the
            // transport with the process's own account of why, so its end of
            // the pipe closing is not a story this loop has to tell.
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => {
                terminate_transport(
                    state,
                    claude_error(format!("could not read Claude Code CLI output: {error}")),
                );
                return;
            }
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                terminate_transport(
                    state,
                    claude_error(format!("Claude Code CLI sent malformed JSON: {error}")),
                );
                return;
            }
        };
        if let Err(error) = route_message(message, state).await {
            terminate_transport(state, error);
            return;
        }
    }
}

async fn route_message(message: Value, state: &Arc<TransportState>) -> Result<(), ProviderError> {
    let Some(kind) = message.get("type").and_then(Value::as_str) else {
        return Err(claude_error(
            "Claude Code CLI sent a message without a type field",
        ));
    };
    // Everything except a control response is conversation — turn output, the
    // init message, the CLI's own control requests — which the launched
    // Session's sink consumes, and a discovery leaves unread.
    if kind != "control_response" {
        // A root Turn ends on `result`; a child Turn ends when its task
        // notification settles the Subagent. Either may be emitted as soon as
        // Claude consumes a permission response, so neither may overtake the
        // durable Decision that releases the settlement receipt.
        let settles_session_history =
            kind == "result" || (kind == "system" && message["subtype"] == "task_notification");
        if settles_session_history {
            state.decision_settlements.wait().await;
        }
        if let Some(conversation) = &state.conversation {
            let _ = conversation.send(Ok(ConversationItem::Message(message)));
        }
        return Ok(());
    }
    let Some(response) = message.get("response") else {
        return Ok(());
    };
    let response = match serde_json::from_value::<ControlResponse>(response.clone()) {
        Ok(response) => response,
        Err(error) => {
            // A subtype this build does not know is wire drift to ride out
            // (ADR 0010); a known subtype that fails to decode is a CLI Suru
            // cannot trust.
            let known_subtype = matches!(
                response.get("subtype").and_then(Value::as_str),
                Some("success" | "error")
            );
            if known_subtype {
                return Err(claude_error(format!(
                    "Claude Code CLI sent a malformed control response: {error}"
                )));
            }
            return Ok(());
        }
    };
    let (request_id, result) = match response {
        ControlResponse::Success {
            request_id,
            response,
        } => (request_id, Ok(response.unwrap_or(Value::Null))),
        ControlResponse::Error { request_id, error } => (
            request_id,
            Err(ControlFailure::Refused(claude_error(format!(
                "Claude Code CLI rejected the request: {}",
                concise_remote_message(&error, "unknown control error")
            )))),
        ),
    };
    let pending = state
        .pending
        .lock()
        .expect("Claude pending request lock is not poisoned")
        .remove(&request_id);
    if let Some(pending) = pending {
        let _ = pending.send(result);
    }
    Ok(())
}

fn terminate_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error.mark_session_lost(), true);
}

fn close_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error, false);
}

fn finish_transport(state: &TransportState, error: ProviderError, conversation_lost: bool) {
    if state.terminated.swap(true, Ordering::AcqRel) {
        return;
    }
    state.decision_settlements.release_all();
    let pending = {
        let mut pending = state
            .pending
            .lock()
            .expect("Claude pending request lock is not poisoned");
        pending
            .drain()
            .map(|(_, response)| response)
            .collect::<Vec<_>>()
    };
    for response in pending {
        let _ = response.send(Err(ControlFailure::Failed(error.clone())));
    }
    // An intended close ends the conversation without a story; a lost process
    // is the conversation's failure, told exactly once.
    if conversation_lost && let Some(conversation) = &state.conversation {
        let _ = conversation.send(Err(error));
    }
}
