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
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, mpsc, oneshot},
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
/// control response, and the failure that ends the connection when the process is lost.
pub(super) type ConversationSink = mpsc::UnboundedSender<Result<Value, ProviderError>>;

/// The arguments that put the CLI in stream-json mode: newline-delimited JSON both ways, with the
/// user's filesystem settings left unloaded so a Suru-launched process runs no hooks and starts no
/// MCP servers.
const CLAUDE_STREAM_JSON_ARGS: [&str; 8] = [
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--setting-sources",
    "",
];

type PendingResponse = oneshot::Sender<Result<Value, ProviderError>>;

struct TransportState {
    pending: StdMutex<HashMap<String, PendingResponse>>,
    terminated: AtomicBool,
    conversation: Option<ConversationSink>,
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
    ) -> Result<ClaudeConnection, ProviderError> {
        let spec = HarnessSpec {
            executable: executable.to_owned(),
            args: CLAUDE_STREAM_JSON_ARGS
                .iter()
                .map(OsString::from)
                .chain(args)
                .collect(),
            name: super::CLAUDE_HARNESS_NAME.to_owned(),
            cwd,
        };
        let (process, ProcessStdio { stdin, stdout }) = spawn_harness_process(&spec)?;
        let state = Arc::new(TransportState {
            pending: StdMutex::new(HashMap::new()),
            terminated: AtomicBool::new(false),
            conversation,
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
    ) -> Result<Value, ProviderError> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(claude_error("Claude Code CLI transport has ended").mark_session_lost());
        }
        let key = format!("suru-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (response_tx, response_rx) = oneshot::channel();
        self.state
            .pending
            .lock()
            .expect("Claude pending request lock is not poisoned")
            .insert(key.clone(), response_tx);
        if let Err(error) = write_json_line(
            &self.writer,
            &ControlRequestEnvelope::new(&key, request),
            "write to Claude Code CLI",
        )
        .await
        {
            self.state
                .pending
                .lock()
                .expect("Claude pending request lock is not poisoned")
                .remove(&key);
            return Err(error);
        }

        match timeout(request_timeout, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(claude_error(
                "Claude Code CLI ended before the control request completed",
            )
            .mark_session_lost()),
            Err(_) => {
                self.state
                    .pending
                    .lock()
                    .expect("Claude pending request lock is not poisoned")
                    .remove(&key);
                Err(claude_error(
                    "Claude Code CLI timed out handling the control request",
                ))
            }
        }
    }

    /// Writes one conversation message — a user message carrying a Prompt — to the CLI's stdin.
    pub(super) async fn send<T: serde::Serialize>(&self, message: &T) -> Result<(), ProviderError> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(claude_error("Claude Code CLI transport has ended").mark_session_lost());
        }
        write_json_line(&self.writer, message, "write to Claude Code CLI").await
    }

    pub(super) async fn close(&self) {
        close_transport(
            &self.state,
            claude_error("Claude Code CLI transport closed during shutdown"),
        );
        close_stdin(&self.writer).await;
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
                    &state,
                    claude_error(format!("could not read Claude Code CLI output: {error}")),
                );
                return;
            }
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                terminate_transport(
                    &state,
                    claude_error(format!("Claude Code CLI sent malformed JSON: {error}")),
                );
                return;
            }
        };
        if let Err(error) = route_message(message, &state) {
            terminate_transport(&state, error);
            return;
        }
    }
}

fn route_message(message: Value, state: &Arc<TransportState>) -> Result<(), ProviderError> {
    let Some(kind) = message.get("type").and_then(Value::as_str) else {
        return Err(claude_error(
            "Claude Code CLI sent a message without a type field",
        ));
    };
    // Everything except a control response is conversation — turn output, the
    // init message, the CLI's own control requests — which the launched
    // Session's sink consumes, and a discovery leaves unread.
    if kind != "control_response" {
        if let Some(conversation) = &state.conversation {
            let _ = conversation.send(Ok(message));
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
            Err(claude_error(format!(
                "Claude Code CLI rejected the request: {}",
                concise_remote_message(&error, "unknown control error")
            ))),
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
        let _ = response.send(Err(error.clone()));
    }
    // An intended close ends the conversation without a story; a lost process
    // is the conversation's failure, told exactly once.
    if conversation_lost && let Some(conversation) = &state.conversation {
        let _ = conversation.send(Err(error));
    }
}
