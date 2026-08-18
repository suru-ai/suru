//! Codex app-server Provider runtime over its stable V2 stdio protocol.

use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot, watch},
    time::{Duration, timeout},
};

use super::{
    ProviderError, ProviderEvent, ProviderEventStream, ProviderFuture, ProviderRuntime,
    ProviderSession, ProviderSessionConnection, ProviderSessionRequest, ProviderTurnInput,
};
use crate::protocol::{AgentId, AgentIdentity, ModelId, ProviderId};

const CODEX_PATH_ENV: &str = "CHIDORI_CODEX_PATH";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REMOTE_ERROR_CHARS: usize = 384;

#[derive(Serialize)]
struct ClientRequest<'a, T> {
    id: i64,
    method: &'a str,
    params: T,
}

#[derive(Serialize)]
struct ClientNotification<'a> {
    method: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    client_info: ClientInfo,
    capabilities: InitializeCapabilities,
}

#[derive(Serialize)]
struct ClientInfo {
    name: &'static str,
    title: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilities {
    experimental_api: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStartParams<'a> {
    cwd: &'a str,
    approval_policy: &'static str,
    sandbox: &'static str,
    ephemeral: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams<'a> {
    thread_id: &'a str,
    input: [TextInput<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnSteerParams<'a> {
    thread_id: &'a str,
    input: [TextInput<'a>; 1],
    expected_turn_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnInterruptParams<'a> {
    thread_id: &'a str,
    turn_id: &'a str,
}

#[derive(Serialize)]
struct TextInput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum RequestId {
    String(String),
    Integer(i64),
    Unsigned(u64),
}

impl RequestId {
    fn correlation_key(&self) -> String {
        match self {
            Self::String(id) => id.clone(),
            Self::Integer(id) => id.to_string(),
            Self::Unsigned(id) => id.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct IncomingMessage {
    id: Option<RequestId>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<RemoteError>,
}

#[derive(Deserialize)]
struct RemoteError {
    message: String,
}

#[derive(Serialize)]
struct ClientErrorResponse {
    id: RequestId,
    error: ClientError,
}

#[derive(Serialize)]
struct ClientError {
    code: i64,
    message: String,
}

/// Launches one Codex app-server process for each Chidori Session.
#[derive(Clone, Debug)]
pub struct CodexRuntime {
    executable: OsString,
}

impl CodexRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
        }
    }

    pub fn from_environment() -> Self {
        let executable = std::env::var_os(CODEX_PATH_ENV)
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| OsString::from("codex"));
        Self { executable }
    }
}

impl Default for CodexRuntime {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl ProviderRuntime for CodexRuntime {
    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        let executable = self.executable.clone();
        Box::pin(async move { start_codex_session(executable, request).await })
    }
}

async fn start_codex_session(
    executable: OsString,
    request: ProviderSessionRequest,
) -> Result<ProviderSessionConnection, ProviderError> {
    let mut child = Command::new(&executable)
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            codex_error(format!(
                "could not launch Codex app-server `{}`: {error}",
                executable.to_string_lossy()
            ))
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| codex_error("Codex app-server stdin was unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| codex_error("Codex app-server stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| codex_error("Codex app-server stderr was unavailable"))?;

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let state = Arc::new(TransportState {
        pending: StdMutex::new(HashMap::new()),
        events: events_tx,
        terminated: AtomicBool::new(false),
    });
    let writer = Arc::new(Mutex::new(stdin));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (exit_tx, exit_rx) = watch::channel(None::<ProviderError>);
    let process = Arc::new(ProcessGuard {
        shutdown: shutdown_tx,
    });

    tokio::spawn(read_stdout(stdout, writer.clone(), state.clone(), exit_rx));
    let process_state = state.clone();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut stderr, &mut sink).await;
    });
    tokio::spawn(async move {
        tokio::select! {
            status = child.wait() => {
                let message = match status {
                    Ok(status) if status.success() => {
                        "Codex app-server exited unexpectedly".to_owned()
                    }
                    Ok(status) => format!("Codex app-server exited unexpectedly with {status}"),
                    Err(error) => format!("could not wait for Codex app-server: {error}"),
                };
                let error = codex_error(message);
                exit_tx.send_replace(Some(error.clone()));
                terminate_transport(&process_state, error);
            }
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        }
    });

    let transport = JsonRpcTransport {
        writer,
        state,
        next_id: Arc::new(AtomicI64::new(1)),
        _process: process.clone(),
    };
    transport
        .request(
            "initialize",
            &InitializeParams {
                client_info: ClientInfo {
                    name: "chidori",
                    title: "Chidori",
                    version: env!("CARGO_PKG_VERSION"),
                },
                capabilities: InitializeCapabilities {
                    experimental_api: false,
                },
            },
        )
        .await
        .map_err(|error| codex_error(format!("Codex initialization failed: {error}")))?;
    transport
        .notify("initialized")
        .await
        .map_err(|error| codex_error(format!("Codex initialization failed: {error}")))?;

    let cwd = request
        .workspace
        .to_str()
        .ok_or_else(|| codex_error("Workspace path cannot be represented for Codex app-server"))?;
    let result = transport
        .request(
            "thread/start",
            &ThreadStartParams {
                cwd,
                approval_policy: "never",
                sandbox: "danger-full-access",
                ephemeral: false,
            },
        )
        .await
        .map_err(|error| codex_error(format!("Codex Session startup failed: {error}")))?;
    let started: ThreadStartResult = serde_json::from_value(result).map_err(|error| {
        codex_error(format!(
            "Codex returned an invalid thread/start response: {error}"
        ))
    })?;
    if started.thread.id.is_empty() {
        return Err(codex_error(
            "Codex returned an invalid thread/start response: Provider Session ID was empty",
        ));
    }
    if started.model.is_empty() {
        return Err(codex_error(
            "Codex returned an invalid thread/start response: effective Model was empty",
        ));
    }

    let correlation = Arc::new(StdMutex::new(NativeCorrelation {
        thread_id: started.thread.id.clone(),
        active_turn_id: None,
        active_agent_message: None,
    }));
    let session = Arc::new(CodexSession {
        thread_id: started.thread.id,
        transport,
        correlation: correlation.clone(),
    });
    let events: ProviderEventStream = Box::pin(stream::unfold(
        GuardedEventReceiver {
            receiver: events_rx,
            _process: process,
            correlation,
            pending: VecDeque::new(),
        },
        next_provider_event,
    ));
    Ok(ProviderSessionConnection::new(
        AgentIdentity {
            agent: AgentId::new("codex"),
            provider: ProviderId::new("codex"),
            model: ModelId::new(started.model),
        },
        session,
        events,
    ))
}

#[derive(Deserialize)]
struct ThreadStartResult {
    thread: NativeThread,
    model: String,
}

#[derive(Deserialize)]
struct NativeThread {
    id: String,
}

struct CodexSession {
    thread_id: String,
    transport: JsonRpcTransport,
    correlation: Arc<StdMutex<NativeCorrelation>>,
}

impl ProviderSession for CodexSession {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let result = self
                .transport
                .request(
                    "turn/start",
                    &TurnStartParams {
                        thread_id: &self.thread_id,
                        input: [TextInput {
                            kind: "text",
                            text: &input.prompt,
                        }],
                    },
                )
                .await
                .map_err(|error| codex_error(format!("Codex Turn startup failed: {error}")))?;
            let started: TurnStartResult = serde_json::from_value(result).map_err(|error| {
                codex_error(format!(
                    "Codex returned an invalid turn/start response: {error}"
                ))
            })?;
            if started.turn.id.is_empty() {
                return Err(codex_error(
                    "Codex returned an invalid turn/start response: Turn ID was empty",
                ));
            }
            let mut correlation = self
                .correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned");
            if correlation.active_turn_id.is_some() {
                return Err(codex_error(
                    "Codex started a Turn while another native Turn was active",
                ));
            }
            correlation.active_turn_id = Some(started.turn.id);
            correlation.active_agent_message = None;
            Ok(())
        })
    }

    fn steer_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let turn_id = self
                .correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned")
                .active_turn_id
                .clone()
                .ok_or_else(|| codex_error("Codex has no active Turn to steer"))?;
            let result = self
                .transport
                .request(
                    "turn/steer",
                    &TurnSteerParams {
                        thread_id: &self.thread_id,
                        input: [TextInput {
                            kind: "text",
                            text: &input.prompt,
                        }],
                        expected_turn_id: &turn_id,
                    },
                )
                .await
                .map_err(|error| codex_error(format!("Codex Turn steering failed: {error}")))?;
            let steered: TurnSteerResult = serde_json::from_value(result).map_err(|error| {
                codex_error(format!(
                    "Codex returned an invalid turn/steer response: {error}"
                ))
            })?;
            if steered.turn_id != turn_id {
                return Err(codex_error(
                    "Codex returned an invalid turn/steer response: Turn ID did not match the active Turn",
                ));
            }
            Ok(())
        })
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let turn_id = self
                .correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned")
                .active_turn_id
                .clone()
                .ok_or_else(|| codex_error("Codex has no active Turn to interrupt"))?;
            self.transport
                .request_with_timeout(
                    "turn/interrupt",
                    &TurnInterruptParams {
                        thread_id: &self.thread_id,
                        turn_id: &turn_id,
                    },
                    INTERRUPT_REQUEST_TIMEOUT,
                )
                .await
                .map_err(|error| codex_error(format!("Codex Turn interruption failed: {error}")))?;
            Ok(())
        })
    }
}

#[derive(Deserialize)]
struct TurnStartResult {
    turn: NativeTurn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnSteerResult {
    turn_id: String,
}

#[derive(Deserialize)]
struct NativeTurn {
    id: String,
}

struct NativeCorrelation {
    thread_id: String,
    active_turn_id: Option<String>,
    active_agent_message: Option<ActiveNativeAgentMessage>,
}

struct ActiveNativeAgentMessage {
    item_id: String,
    streamed_text: String,
}

enum NativeNotification {
    AgentMessageStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    AgentMessageCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        text: String,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        outcome: NativeTurnOutcome,
    },
}

enum NativeTurnOutcome {
    Completed,
    Interrupted,
    Failed { message: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNotificationParams {
    thread_id: String,
    turn_id: String,
    item: NativeItem,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum NativeItem {
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentMessageDeltaParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCompletedParams {
    thread_id: String,
    turn: CompletedNativeTurn,
}

#[derive(Deserialize)]
struct CompletedNativeTurn {
    id: String,
    status: NativeTurnStatus,
    error: Option<NativeTurnError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum NativeTurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Deserialize)]
struct NativeTurnError {
    message: String,
}

struct GuardedEventReceiver {
    receiver: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    _process: Arc<ProcessGuard>,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: GuardedEventReceiver,
) -> Option<(Result<ProviderEvent, ProviderError>, GuardedEventReceiver)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let native = events.receiver.recv().await?;
        match native {
            Err(error) => return Some((Err(error), events)),
            Ok(native) => {
                let projected = {
                    let mut correlation = events
                        .correlation
                        .lock()
                        .expect("Codex native correlation lock is not poisoned");
                    project_native_notification(&mut correlation, native)
                };
                match projected {
                    Ok(projected) => events.pending.extend(projected.into_iter().map(Ok)),
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

fn project_native_notification(
    correlation: &mut NativeCorrelation,
    notification: NativeNotification,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    match notification {
        NativeNotification::AgentMessageStarted {
            thread_id,
            turn_id,
            item_id,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            if correlation.active_agent_message.is_some() {
                return Err(codex_error(
                    "Codex started a second Agent Message before completing the first",
                ));
            }
            correlation.active_agent_message = Some(ActiveNativeAgentMessage {
                item_id,
                streamed_text: String::new(),
            });
            Ok(vec![ProviderEvent::AgentMessageStarted])
        }
        NativeNotification::AgentMessageDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(message) = correlation.active_agent_message.as_mut() else {
                return Err(codex_error(
                    "Codex sent Agent Message content before starting the Message",
                ));
            };
            if message.item_id != item_id {
                return Ok(Vec::new());
            }
            message.streamed_text.push_str(&delta);
            Ok(vec![ProviderEvent::AgentMessageDelta { content: delta }])
        }
        NativeNotification::AgentMessageCompleted {
            thread_id,
            turn_id,
            item_id,
            text,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(message) = correlation.active_agent_message.as_ref() else {
                return Err(codex_error(
                    "Codex completed an Agent Message before starting it",
                ));
            };
            if message.item_id != item_id {
                return Ok(Vec::new());
            }
            let Some(remaining) = text.strip_prefix(&message.streamed_text) else {
                return Err(codex_error(
                    "Codex completed an Agent Message with content that did not match its stream",
                ));
            };
            let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
            if !remaining.is_empty() {
                projected.push(ProviderEvent::AgentMessageDelta {
                    content: remaining.to_owned(),
                });
            }
            projected.push(ProviderEvent::AgentMessageCompleted);
            correlation.active_agent_message = None;
            Ok(projected)
        }
        NativeNotification::TurnCompleted {
            thread_id,
            turn_id,
            outcome,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            correlation.active_turn_id = None;
            correlation.active_agent_message = None;
            Ok(vec![match outcome {
                NativeTurnOutcome::Completed => ProviderEvent::TurnCompleted,
                NativeTurnOutcome::Interrupted => ProviderEvent::TurnInterrupted,
                NativeTurnOutcome::Failed { message } => ProviderEvent::TurnFailed { message },
            }])
        }
    }
}

fn is_active_native_turn(correlation: &NativeCorrelation, thread_id: &str, turn_id: &str) -> bool {
    correlation.thread_id == thread_id && correlation.active_turn_id.as_deref() == Some(turn_id)
}

struct ProcessGuard {
    shutdown: watch::Sender<bool>,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

type PendingResponse = oneshot::Sender<Result<Value, ProviderError>>;

struct TransportState {
    pending: StdMutex<HashMap<String, PendingResponse>>,
    events: mpsc::UnboundedSender<Result<NativeNotification, ProviderError>>,
    terminated: AtomicBool,
}

#[derive(Clone)]
struct JsonRpcTransport {
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
    state: Arc<TransportState>,
    next_id: Arc<AtomicI64>,
    _process: Arc<ProcessGuard>,
}

impl JsonRpcTransport {
    async fn request<T: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<Value, ProviderError> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    async fn request_with_timeout<T: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &T,
        request_timeout: Duration,
    ) -> Result<Value, ProviderError> {
        if self.state.terminated.load(Ordering::Acquire) {
            return Err(codex_error("Codex app-server transport has ended"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let key = id.to_string();
        let (response_tx, response_rx) = oneshot::channel();
        self.state
            .pending
            .lock()
            .expect("Codex pending request lock is not poisoned")
            .insert(key.clone(), response_tx);
        if let Err(error) = write_json_line(
            &self.writer,
            &ClientRequest { id, method, params },
            "write to Codex app-server",
        )
        .await
        {
            self.state
                .pending
                .lock()
                .expect("Codex pending request lock is not poisoned")
                .remove(&key);
            return Err(error);
        }

        match timeout(request_timeout, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(codex_error(format!(
                "Codex app-server ended before `{method}` completed"
            ))),
            Err(_) => {
                self.state
                    .pending
                    .lock()
                    .expect("Codex pending request lock is not poisoned")
                    .remove(&key);
                Err(codex_error(format!(
                    "Codex app-server timed out handling `{method}`"
                )))
            }
        }
    }

    async fn notify(&self, method: &str) -> Result<(), ProviderError> {
        write_json_line(
            &self.writer,
            &ClientNotification { method },
            "write to Codex app-server",
        )
        .await
    }
}

async fn write_json_line<T: Serialize + ?Sized>(
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    value: &T,
    operation: &str,
) -> Result<(), ProviderError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| codex_error(format!("could not encode Codex message: {error}")))?;
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| codex_error(format!("could not {operation}: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| codex_error(format!("could not flush after {operation}: {error}")))
}

async fn read_stdout(
    stdout: ChildStdout,
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
    state: Arc<TransportState>,
    mut exit: watch::Receiver<Option<ProviderError>>,
) {
    let mut lines = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(0) => {
                let error = await_exit_error(&mut exit).await.unwrap_or_else(|| {
                    codex_error("Codex app-server closed its output unexpectedly")
                });
                terminate_transport(&state, error);
                return;
            }
            Ok(_) => {}
            Err(error) => {
                terminate_transport(
                    &state,
                    codex_error(format!("could not read Codex app-server output: {error}")),
                );
                return;
            }
        }
        let message: IncomingMessage = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                terminate_transport(
                    &state,
                    codex_error(format!("Codex app-server sent malformed JSON: {error}")),
                );
                return;
            }
        };
        if let Err(error) = route_message(message, &writer, &state).await {
            terminate_transport(&state, error);
            return;
        }
    }
}

async fn await_exit_error(
    exit: &mut watch::Receiver<Option<ProviderError>>,
) -> Option<ProviderError> {
    if let Some(error) = exit.borrow().clone() {
        return Some(error);
    }
    timeout(Duration::from_millis(100), async {
        loop {
            exit.changed().await.ok()?;
            if let Some(error) = exit.borrow_and_update().clone() {
                return Some(error);
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn route_message(
    message: IncomingMessage,
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    state: &Arc<TransportState>,
) -> Result<(), ProviderError> {
    if let Some(method) = message.method.as_deref() {
        if let Some(id) = message.id {
            reject_server_request(writer, id, method).await?;
            return Err(codex_error(format!(
                "Codex app-server requested unsupported interaction `{method}`"
            )));
        }
        if let Some(event) = decode_notification(method, message.params.as_ref())? {
            let _ = state.events.send(Ok(event));
        }
        return Ok(());
    }

    let id = message
        .id
        .ok_or_else(|| codex_error("Codex app-server sent a message without a method or ID"))?
        .correlation_key();
    let pending = state
        .pending
        .lock()
        .expect("Codex pending request lock is not poisoned")
        .remove(&id);
    let Some(pending) = pending else {
        return Ok(());
    };
    let response = if let Some(error) = message.error {
        Err(codex_error(format!(
            "Codex app-server rejected the request: {}",
            concise_remote_message(&error.message, "unknown protocol error")
        )))
    } else if let Some(result) = message.result {
        Ok(result)
    } else {
        Err(codex_error(
            "Codex app-server response contained neither a result nor an error",
        ))
    };
    let _ = pending.send(response);
    Ok(())
}

async fn reject_server_request(
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    id: RequestId,
    method: &str,
) -> Result<(), ProviderError> {
    write_json_line(
        writer,
        &ClientErrorResponse {
            id,
            error: ClientError {
                code: -32601,
                message: format!("Chidori does not support `{method}` requests"),
            },
        },
        "reject Codex app-server request",
    )
    .await
}

fn decode_notification(
    method: &str,
    params: Option<&Value>,
) -> Result<Option<NativeNotification>, ProviderError> {
    match method {
        "item/started" => {
            let params: ItemNotificationParams = decode_notification_params(method, params)?;
            match params.item {
                NativeItem::AgentMessage { id, .. } => {
                    Ok(Some(NativeNotification::AgentMessageStarted {
                        thread_id: params.thread_id,
                        turn_id: params.turn_id,
                        item_id: id,
                    }))
                }
                NativeItem::Unknown => Ok(None),
            }
        }
        "item/agentMessage/delta" => {
            let params: AgentMessageDeltaParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::AgentMessageDelta {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                delta: params.delta,
            }))
        }
        "item/completed" => {
            let params: ItemNotificationParams = decode_notification_params(method, params)?;
            match params.item {
                NativeItem::AgentMessage { id, text } => {
                    Ok(Some(NativeNotification::AgentMessageCompleted {
                        thread_id: params.thread_id,
                        turn_id: params.turn_id,
                        item_id: id,
                        text,
                    }))
                }
                NativeItem::Unknown => Ok(None),
            }
        }
        "turn/completed" => {
            let params: TurnCompletedParams = decode_notification_params(method, params)?;
            let outcome = match params.turn.status {
                NativeTurnStatus::Completed => NativeTurnOutcome::Completed,
                NativeTurnStatus::Interrupted => NativeTurnOutcome::Interrupted,
                NativeTurnStatus::Failed => NativeTurnOutcome::Failed {
                    message: concise_remote_message(
                        params
                            .turn
                            .error
                            .as_ref()
                            .map(|error| error.message.as_str())
                            .unwrap_or("Codex Turn failed"),
                        "Codex Turn failed",
                    ),
                },
            };
            Ok(Some(NativeNotification::TurnCompleted {
                thread_id: params.thread_id,
                turn_id: params.turn.id,
                outcome,
            }))
        }
        _ => Ok(None),
    }
}

fn decode_notification_params<T: for<'de> Deserialize<'de>>(
    method: &str,
    params: Option<&Value>,
) -> Result<T, ProviderError> {
    let params = params.ok_or_else(|| {
        codex_error(format!(
            "Codex app-server notification `{method}` omitted params"
        ))
    })?;
    serde_json::from_value(params.clone()).map_err(|error| {
        codex_error(format!(
            "Codex app-server notification `{method}` had invalid params: {error}"
        ))
    })
}

fn concise_remote_message(message: &str, fallback: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let message = if single_line.is_empty() {
        fallback
    } else {
        &single_line
    };
    let mut chars = message.chars();
    let mut concise = chars
        .by_ref()
        .take(MAX_REMOTE_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        concise.push('…');
    }
    concise
}

fn codex_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        "Codex Provider failed",
    ))
}

fn terminate_transport(state: &TransportState, error: ProviderError) {
    if state.terminated.swap(true, Ordering::AcqRel) {
        return;
    }
    let pending = {
        let mut pending = state
            .pending
            .lock()
            .expect("Codex pending request lock is not poisoned");
        pending
            .drain()
            .map(|(_, response)| response)
            .collect::<Vec<_>>()
    };
    for response in pending {
        let _ = response.send(Err(error.clone()));
    }
    let _ = state.events.send(Err(error));
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use super::{CODEX_PATH_ENV, CodexRuntime};

    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_uses_the_override_or_codex_from_path() {
        let _environment = ENVIRONMENT
            .lock()
            .expect("Codex environment test lock is not poisoned");
        let original = std::env::var_os(CODEX_PATH_ENV);

        // SAFETY: this unit test serializes every mutation of this process variable and restores it
        // before releasing the lock. No production task is running in the unit-test process.
        unsafe {
            std::env::set_var(CODEX_PATH_ENV, "/fixture/custom-codex");
        }
        assert_eq!(
            CodexRuntime::from_environment().executable,
            OsString::from("/fixture/custom-codex")
        );

        // SAFETY: covered by the serialized test scope described above.
        unsafe {
            std::env::remove_var(CODEX_PATH_ENV);
        }
        assert_eq!(
            CodexRuntime::from_environment().executable,
            OsString::from("codex")
        );

        // SAFETY: restore the exact environment observed before the serialized test scope.
        unsafe {
            if let Some(original) = original {
                std::env::set_var(CODEX_PATH_ENV, original);
            } else {
                std::env::remove_var(CODEX_PATH_ENV);
            }
        }
    }
}
