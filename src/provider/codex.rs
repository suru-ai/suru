//! Codex app-server Provider runtime over its stable V2 stdio protocol.

use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
};

use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Notify, mpsc, oneshot, watch},
    time::{Duration, timeout},
};

use super::{
    ProviderActivityId, ProviderCommandStatus, ProviderError, ProviderEvent, ProviderEventStream,
    ProviderFileChangeStatus, ProviderFuture, ProviderRuntime, ProviderSession,
    ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
    wait_for_shutdown,
};
use crate::protocol::{
    AgentId, AgentIdentity, AgentSelection, FileChange, ModelAvailability, ModelDescriptor,
    ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId,
    ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue, ProviderId,
    SessionId,
};

const CODEX_PATH_ENV: &str = "CHIDORI_CODEX_PATH";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
const PENDING_TURN_START_GRACE_PERIOD: Duration = Duration::from_millis(250);
const PROCESS_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(500);
const PROCESS_KILL_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_REMOTE_ERROR_CHARS: usize = 384;
const UNSUPPORTED_INTERACTION_ERROR_CODE: i64 = -32000;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const REASONING_EFFORT_OPTION_ID: &str = "reasoning_effort";
const SERVICE_TIER_OPTION_ID: &str = "service_tier";
const DEFAULT_SERVICE_TIER_CHOICE_ID: &str = "default";

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
struct ModelListParams<'a> {
    cursor: Option<&'a str>,
    limit: Option<u32>,
    include_hidden: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModelList {
    data: Vec<NativeModel>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModel {
    id: String,
    display_name: String,
    description: String,
    hidden: bool,
    supported_reasoning_efforts: Vec<NativeReasoningEffort>,
    default_reasoning_effort: String,
    #[serde(default)]
    service_tiers: Vec<NativeServiceTier>,
    #[serde(default)]
    default_service_tier: Option<String>,
    is_default: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeReasoningEffort {
    reasoning_effort: String,
    description: String,
}

#[derive(Deserialize)]
struct NativeServiceTier {
    id: String,
    name: String,
    description: String,
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
struct ThreadResumeParams<'a> {
    thread_id: &'a str,
    cwd: &'a str,
    approval_policy: &'static str,
    sandbox: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartParams<'a> {
    thread_id: &'a str,
    input: [TextInput<'a>; 1],
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    #[serde(
        skip_serializing_if = "NativeServiceTierOverride::is_omitted",
        serialize_with = "serialize_native_service_tier"
    )]
    service_tier: NativeServiceTierOverride<'a>,
}

struct NativeTurnOptions<'a> {
    effort: Option<&'a str>,
    service_tier: NativeServiceTierOverride<'a>,
}

enum NativeServiceTierOverride<'a> {
    Omitted,
    Clear,
    Value(&'a str),
}

impl NativeServiceTierOverride<'_> {
    fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

fn serialize_native_service_tier<S>(
    service_tier: &NativeServiceTierOverride<'_>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match service_tier {
        NativeServiceTierOverride::Omitted | NativeServiceTierOverride::Clear => {
            serializer.serialize_none()
        }
        NativeServiceTierOverride::Value(value) => serializer.serialize_str(value),
    }
}

fn lower_turn_options(selection: &AgentSelection) -> Result<NativeTurnOptions<'_>, ProviderError> {
    let mut effort = None;
    let mut service_tier = NativeServiceTierOverride::Omitted;
    for option in &selection.options {
        let ModelOptionValue::Select { choice } = &option.value else {
            return Err(ProviderError::selection_rejected(format!(
                "Codex does not support toggle Model Option `{}`",
                option.id
            )));
        };
        match option.id.as_str() {
            REASONING_EFFORT_OPTION_ID if effort.is_none() => {
                effort = Some(choice.as_str());
            }
            SERVICE_TIER_OPTION_ID if service_tier.is_omitted() => {
                service_tier = if choice.as_str() == DEFAULT_SERVICE_TIER_CHOICE_ID {
                    NativeServiceTierOverride::Clear
                } else {
                    NativeServiceTierOverride::Value(choice.as_str())
                };
            }
            REASONING_EFFORT_OPTION_ID | SERVICE_TIER_OPTION_ID => {
                return Err(ProviderError::selection_rejected(format!(
                    "Codex Model Option `{}` was selected more than once",
                    option.id
                )));
            }
            _ => {
                return Err(ProviderError::selection_rejected(format!(
                    "Codex does not support Model Option `{}`",
                    option.id
                )));
            }
        }
    }
    Ok(NativeTurnOptions {
        effort,
        service_tier,
    })
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
    processes: ProcessRegistry,
    thread_ids_by_session: Arc<StdMutex<HashMap<SessionId, CodexThreadId>>>,
}

#[derive(Clone, Debug)]
struct CodexThreadId(String);

impl CodexRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(),
            thread_ids_by_session: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub fn from_environment() -> Self {
        let executable = std::env::var_os(CODEX_PATH_ENV)
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| OsString::from("codex"));
        Self::new(executable)
    }
}

impl Default for CodexRuntime {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl ProviderRuntime for CodexRuntime {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new("codex")
    }

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        Box::pin(async move { discover_codex_models(executable, processes).await })
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let thread_ids_by_session = self.thread_ids_by_session.clone();
        Box::pin(async move {
            start_codex_session(executable, request, processes, thread_ids_by_session).await
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.processes.shutdown().await })
    }
}

async fn discover_codex_models(
    executable: OsString,
    processes: ProcessRegistry,
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let (transport, _events, process) =
        start_initialized_codex_transport(executable, processes).await?;
    let mut cursor = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut models = Vec::new();
    loop {
        let result = transport
            .request(
                "model/list",
                &ModelListParams {
                    cursor: cursor.as_deref(),
                    limit: None,
                    include_hidden: Some(true),
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Model discovery failed", error))?;
        let page: NativeModelList = serde_json::from_value(result).map_err(|error| {
            codex_error(format!(
                "Codex returned an invalid model/list response: {error}"
            ))
        })?;
        models.extend(
            page.data
                .into_iter()
                .filter(|model| !model.hidden)
                .map(normalize_model),
        );
        match page.next_cursor {
            Some(next) if next.is_empty() => {
                return Err(codex_error(
                    "Codex returned an invalid model/list response: pagination cursor was empty",
                ));
            }
            Some(next) if !seen_cursors.insert(next.clone()) => {
                return Err(codex_error(
                    "Codex returned an invalid model/list response: pagination cursor did not advance",
                ));
            }
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    transport.close().await;
    process.wait_until_stopped().await?;
    Ok(models)
}

fn normalize_model(model: NativeModel) -> ModelDescriptor {
    let provider = ProviderId::new("codex");
    let mut options = Vec::new();
    if !model.supported_reasoning_efforts.is_empty() {
        options.push(ModelOptionDescriptor {
            id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
            label: "Reasoning effort".to_owned(),
            description: None,
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices: model
                    .supported_reasoning_efforts
                    .into_iter()
                    .map(|effort| ModelOptionChoice {
                        label: title_case_id(&effort.reasoning_effort),
                        id: ModelOptionChoiceId::new(effort.reasoning_effort),
                        description: Some(effort.description),
                        availability: ModelAvailability::Available,
                    })
                    .collect(),
                default: ModelOptionChoiceId::new(model.default_reasoning_effort),
            },
        });
    }
    if !model.service_tiers.is_empty() {
        let mut choices = model
            .service_tiers
            .into_iter()
            .map(|tier| ModelOptionChoice {
                id: ModelOptionChoiceId::new(tier.id),
                label: tier.name,
                description: Some(tier.description),
                availability: ModelAvailability::Available,
            })
            .collect::<Vec<_>>();
        let default = model
            .default_service_tier
            .unwrap_or_else(|| "default".to_owned());
        if !choices.iter().any(|choice| choice.id.as_str() == default) && default == "default" {
            choices.insert(
                0,
                ModelOptionChoice {
                    id: ModelOptionChoiceId::new(DEFAULT_SERVICE_TIER_CHOICE_ID),
                    label: "Default".to_owned(),
                    description: None,
                    availability: ModelAvailability::Available,
                },
            );
        }
        options.push(ModelOptionDescriptor {
            id: ModelOptionId::new(SERVICE_TIER_OPTION_ID),
            label: "Speed".to_owned(),
            description: None,
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Select {
                choices,
                default: ModelOptionChoiceId::new(default),
            },
        });
    }
    ModelDescriptor {
        provider,
        id: ModelId::new(model.id),
        display_name: model.display_name,
        description: model.description,
        is_default: model.is_default,
        availability: ModelAvailability::Available,
        options,
    }
}

fn title_case_id(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

async fn start_codex_session(
    executable: OsString,
    request: ProviderSessionRequest,
    processes: ProcessRegistry,
    thread_ids_by_session: Arc<StdMutex<HashMap<SessionId, CodexThreadId>>>,
) -> Result<ProviderSessionConnection, ProviderError> {
    let (transport, events_rx, process) =
        start_initialized_codex_transport(executable, processes).await?;
    start_codex_thread(
        transport,
        events_rx,
        process,
        request,
        thread_ids_by_session,
    )
    .await
}

async fn start_initialized_codex_transport(
    executable: OsString,
    processes: ProcessRegistry,
) -> Result<
    (
        JsonRpcTransport,
        mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
        Arc<ProcessGuard>,
    ),
    ProviderError,
> {
    let mut command = Command::new(&executable);
    command
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let (mut child, process_tree) = spawn_codex_child(&mut command).map_err(|error| {
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
    let writer = Arc::new(Mutex::new(Some(stdin)));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped_rx) = watch::channel(false);
    let (exit_tx, exit_rx) = watch::channel(None::<ProviderError>);
    let process_control = ProcessControl {
        shutdown: shutdown_tx,
        stopped: stopped_rx,
    };
    let registration_id = match processes.register(process_control.clone()) {
        Ok(registration_id) => registration_id,
        Err(error) => {
            let _ = process_tree.terminate(&mut child);
            let _ = timeout(PROCESS_KILL_TIMEOUT, child.wait()).await;
            return Err(error);
        }
    };
    let process = Arc::new(ProcessGuard {
        control: process_control,
    });

    tokio::spawn(read_stdout(stdout, writer.clone(), state.clone(), exit_rx));
    let process_state = state.clone();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut stderr, &mut sink).await;
    });
    tokio::spawn(supervise_child(ChildSupervisor {
        child,
        shutdown: shutdown_rx,
        stopped: stopped_tx,
        exit: exit_tx,
        writer: writer.clone(),
        state: process_state,
        processes,
        registration_id,
        process_tree,
    }));

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
        .map_err(|error| codex_error_context("Codex initialization failed", error))?;
    transport
        .notify("initialized")
        .await
        .map_err(|error| codex_error_context("Codex initialization failed", error))?;

    Ok((transport, events_rx, process))
}

async fn start_codex_thread(
    transport: JsonRpcTransport,
    events_rx: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    process: Arc<ProcessGuard>,
    request: ProviderSessionRequest,
    thread_ids_by_session: Arc<StdMutex<HashMap<SessionId, CodexThreadId>>>,
) -> Result<ProviderSessionConnection, ProviderError> {
    let cwd = request
        .workspace
        .to_str()
        .ok_or_else(|| codex_error("Workspace path cannot be represented for Codex app-server"))?;
    let known_thread_id = thread_ids_by_session
        .lock()
        .expect("Codex Thread registry lock is not poisoned")
        .get(&request.session_id)
        .cloned();
    let (method, result) = if let Some(thread_id) = known_thread_id.as_ref() {
        let result = transport
            .request(
                "thread/resume",
                &ThreadResumeParams {
                    thread_id: &thread_id.0,
                    cwd,
                    approval_policy: "never",
                    sandbox: "danger-full-access",
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Session resume failed", error))?;
        ("thread/resume", result)
    } else {
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
            .map_err(|error| codex_error_context("Codex Session startup failed", error))?;
        ("thread/start", result)
    };
    let started: ThreadConnectionResult = serde_json::from_value(result).map_err(|error| {
        codex_error(format!(
            "Codex returned an invalid {method} response: {error}"
        ))
    })?;
    if started.thread.id.is_empty() {
        return Err(codex_error(format!(
            "Codex returned an invalid {method} response: Provider Session ID was empty"
        )));
    }
    if started.model.is_empty() {
        return Err(codex_error(format!(
            "Codex returned an invalid {method} response: effective Model was empty"
        )));
    }
    if let Some(known_thread_id) = known_thread_id.as_ref()
        && started.thread.id != known_thread_id.0
    {
        return Err(codex_error(
            "Codex returned an invalid thread/resume response: resumed Provider Session ID changed",
        ));
    }
    thread_ids_by_session
        .lock()
        .expect("Codex Thread registry lock is not poisoned")
        .insert(request.session_id, CodexThreadId(started.thread.id.clone()));

    let mut initial_options = Vec::new();
    if let NativeField::Present(Some(effort)) = started.reasoning_effort {
        initial_options.push(ModelOptionSelection {
            id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(effort),
            },
        });
    }
    if let NativeField::Present(service_tier) = started.service_tier {
        initial_options.push(ModelOptionSelection {
            id: ModelOptionId::new(SERVICE_TIER_OPTION_ID),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(
                    service_tier.unwrap_or_else(|| DEFAULT_SERVICE_TIER_CHOICE_ID.to_owned()),
                ),
            },
        });
    }

    let correlation = Arc::new(StdMutex::new(NativeCorrelation {
        thread_id: started.thread.id.clone(),
        turn_starting: false,
        active_turn_id: None,
        active_selection: None,
        active_agent_message: None,
        active_commands: HashMap::new(),
        active_file_changes: HashMap::new(),
    }));
    let turn_start_changed = Arc::new(Notify::new());
    let session = Arc::new(CodexSession {
        thread_id: started.thread.id,
        transport,
        correlation: correlation.clone(),
        turn_start_changed,
        process: process.clone(),
        shutdown_started: AtomicBool::new(false),
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
            selection: AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new(started.model),
                options: initial_options,
            },
        },
        session,
        events,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadConnectionResult {
    thread: NativeThread,
    model: String,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    reasoning_effort: NativeField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    service_tier: NativeField<Option<String>>,
}

#[derive(Deserialize)]
struct NativeThread {
    id: String,
}

struct CodexSession {
    thread_id: String,
    transport: JsonRpcTransport,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    turn_start_changed: Arc<Notify>,
    process: Arc<ProcessGuard>,
    shutdown_started: AtomicBool,
}

impl ProviderSession for CodexSession {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            {
                let mut correlation = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned");
                if self.shutdown_started.load(Ordering::Acquire) {
                    return Err(codex_error("Codex Session is shutting down"));
                }
                if correlation.turn_starting || correlation.active_turn_id.is_some() {
                    return Err(codex_error(
                        "Codex started a Turn while another native Turn was active",
                    ));
                }
                correlation.turn_starting = true;
            }
            let task = tokio::spawn(start_native_turn(
                self.thread_id.clone(),
                input.prompt,
                input.selection,
                self.transport.clone(),
                self.correlation.clone(),
                self.turn_start_changed.clone(),
            ));
            task.await
                .map_err(|error| codex_error(format!("Codex Turn startup task failed: {error}")))?
        })
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
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
                .map_err(|error| codex_error_context("Codex Turn steering failed", error))?;
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
                .map_err(|error| codex_error_context("Codex Turn interruption failed", error))?;
            Ok(())
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if self.shutdown_started.swap(true, Ordering::AcqRel) {
                return self.process.wait_until_stopped().await;
            }

            let turn_start_changed = self.turn_start_changed.notified();
            let (mut active_turn_id, turn_starting) = {
                let correlation = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned");
                (
                    correlation.active_turn_id.clone(),
                    correlation.turn_starting,
                )
            };
            if active_turn_id.is_none() && turn_starting {
                let _ = timeout(PENDING_TURN_START_GRACE_PERIOD, turn_start_changed).await;
                active_turn_id = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned")
                    .active_turn_id
                    .clone();
            }
            if let Some(turn_id) = active_turn_id {
                let _ = timeout(
                    SHUTDOWN_INTERRUPT_REQUEST_TIMEOUT,
                    self.transport.request(
                        "turn/interrupt",
                        &TurnInterruptParams {
                            thread_id: &self.thread_id,
                            turn_id: &turn_id,
                        },
                    ),
                )
                .await;
            }

            self.process.begin_shutdown();
            self.transport.close().await;
            self.process.wait_until_stopped().await
        })
    }
}

async fn start_native_turn(
    thread_id: String,
    prompt: String,
    selection: AgentSelection,
    transport: JsonRpcTransport,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    turn_start_changed: Arc<Notify>,
) -> Result<(), ProviderError> {
    let started = async {
        let options = lower_turn_options(&selection)?;
        let result = transport
            .request(
                "turn/start",
                &TurnStartParams {
                    thread_id: &thread_id,
                    input: [TextInput {
                        kind: "text",
                        text: &prompt,
                    }],
                    model: selection.model.as_str(),
                    effort: options.effort,
                    service_tier: options.service_tier,
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Turn startup failed", error))?;
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
        Ok(started.turn.id)
    }
    .await;

    let result = {
        let mut native = correlation
            .lock()
            .expect("Codex native correlation lock is not poisoned");
        native.turn_starting = false;
        match started {
            Ok(turn_id) if native.active_turn_id.is_none() => {
                native.active_turn_id = Some(turn_id);
                native.active_selection = Some(selection);
                native.active_agent_message = None;
                native.active_commands.clear();
                native.active_file_changes.clear();
                Ok(())
            }
            Ok(_) => Err(codex_error(
                "Codex started a Turn while another native Turn was active",
            )),
            Err(error) => Err(error),
        }
    };
    turn_start_changed.notify_one();
    result
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
    turn_starting: bool,
    active_turn_id: Option<String>,
    active_selection: Option<AgentSelection>,
    active_agent_message: Option<ActiveNativeAgentMessage>,
    active_commands: HashMap<String, ActiveNativeCommand>,
    active_file_changes: HashMap<String, ActiveNativeFileChange>,
}

struct ActiveNativeAgentMessage {
    item_id: String,
    streamed_text: String,
}

struct ActiveNativeCommand {
    streamed_output: String,
}

struct ActiveNativeFileChange {
    changes: Vec<FileChange>,
}

enum NativeNotification {
    AgentSelectionChanged {
        thread_id: String,
        model: String,
        effort: NativeField<Option<String>>,
        service_tier: NativeField<Option<String>>,
    },
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
    CommandStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        command: String,
        cwd: Option<PathBuf>,
        status: NativeCommandStatus,
    },
    CommandOutputDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    CommandCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        aggregated_output: Option<String>,
        exit_status: Option<i32>,
        status: NativeCommandStatus,
    },
    FileChangeStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
    },
    FileChangeUpdated {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
    },
    FileChangeCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
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
    Failed {
        message: String,
        kind: NativeTurnFailureKind,
    },
}

enum NativeTurnFailureKind {
    BadRequest { additional_details: Option<String> },
    Other,
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
    CommandExecution {
        id: String,
        command: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
        status: NativeCommandStatus,
        #[serde(default, rename = "aggregatedOutput")]
        aggregated_output: Option<String>,
        #[serde(default, rename = "exitCode")]
        exit_code: Option<i32>,
    },
    FileChange {
        id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
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

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum NativeCommandStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Clone, Deserialize)]
struct NativeFileChange {
    path: PathBuf,
    kind: NativeFileChangeKind,
}

impl From<NativeFileChange> for FileChange {
    fn from(change: NativeFileChange) -> Self {
        match change.kind {
            NativeFileChangeKind::Add => Self::Add { path: change.path },
            NativeFileChangeKind::Delete => Self::Delete { path: change.path },
            NativeFileChangeKind::Update { move_path } => Self::Update {
                path: change.path,
                moved_to: move_path,
            },
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum NativeFileChangeKind {
    Add,
    Delete,
    Update {
        #[serde(default, rename = "movePath")]
        move_path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum NativeFileChangeStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileChangeUpdatedParams {
    thread_id: String,
    turn_id: String,
    item_id: String,
    changes: Vec<NativeFileChange>,
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
#[serde(rename_all = "camelCase")]
struct NativeTurnError {
    message: String,
    #[serde(default)]
    codex_error_info: Option<NativeCodexErrorInfo>,
    #[serde(default)]
    additional_details: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum NativeCodexErrorInfo {
    BadRequest,
    #[serde(other)]
    Other,
}

fn is_native_selection_rejection(
    message: &str,
    kind: &NativeTurnFailureKind,
    selection: &AgentSelection,
) -> bool {
    match kind {
        NativeTurnFailureKind::BadRequest { additional_details } => additional_details
            .as_deref()
            .and_then(|details| serde_json::from_str::<Value>(details).ok())
            .is_some_and(|details| json_identifies_agent_selection_parameter(&details)),
        NativeTurnFailureKind::Other => {
            let message = message.to_ascii_lowercase();
            let selected_model = selection.model.as_str().to_ascii_lowercase();
            let rejected = [
                "unavailable",
                "unsupported",
                "not available",
                "not found",
                "does not exist",
                "unknown",
                "invalid",
                "access",
                "denied",
                "retired",
            ]
            .iter()
            .any(|reason| message.contains(reason));
            let identifies_model = message.contains("model") && message.contains(&selected_model);
            let identifies_option = selection.options.iter().any(|option| {
                let option_id = option.id.as_str().to_ascii_lowercase();
                let option_label = option_id.replace('_', " ");
                message.contains(&option_id) || message.contains(&option_label)
            });
            rejected && (identifies_model || identifies_option)
        }
    }
}

fn json_identifies_agent_selection_parameter(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("param")
                .and_then(Value::as_str)
                .is_some_and(|parameter| {
                    matches!(
                        parameter,
                        "model"
                            | "effort"
                            | "reasoningEffort"
                            | "reasoning_effort"
                            | "serviceTier"
                            | "service_tier"
                    )
                })
                || fields
                    .values()
                    .any(json_identifies_agent_selection_parameter)
        }
        Value::Array(values) => values.iter().any(json_identifies_agent_selection_parameter),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSettingsUpdatedParams {
    thread_id: String,
    thread_settings: EffectiveThreadSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveThreadSettings {
    model: String,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    effort: NativeField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    service_tier: NativeField<Option<String>>,
}

#[derive(Default)]
enum NativeField<T> {
    #[default]
    Omitted,
    Present(T),
}

fn deserialize_native_field<'de, D, T>(deserializer: D) -> Result<NativeField<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(NativeField::Present)
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
        NativeNotification::AgentSelectionChanged {
            thread_id,
            model,
            effort,
            service_tier,
        } => {
            if correlation.thread_id != thread_id || correlation.active_turn_id.is_none() {
                return Ok(Vec::new());
            }
            if model.is_empty() {
                return Err(codex_error(
                    "Codex reported an empty effective Model for the active Turn",
                ));
            }
            let Some(requested) = correlation.active_selection.as_ref() else {
                return Err(codex_error(
                    "Codex reported effective settings before accepting the active Turn",
                ));
            };
            let mut effective = requested.clone();
            effective.model = ModelId::new(model);
            apply_effective_select_option(
                &mut effective,
                REASONING_EFFORT_OPTION_ID,
                effort,
                NativeClearMapping::RemoveOption,
            )?;
            apply_effective_select_option(
                &mut effective,
                SERVICE_TIER_OPTION_ID,
                service_tier,
                NativeClearMapping::Select(DEFAULT_SERVICE_TIER_CHOICE_ID),
            )?;
            if effective == *requested {
                return Ok(Vec::new());
            }
            correlation.active_selection = Some(effective.clone());
            Ok(vec![ProviderEvent::AgentSelectionChanged {
                selection: effective,
            }])
        }
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
        NativeNotification::CommandStarted {
            thread_id,
            turn_id,
            item_id,
            command,
            cwd,
            status,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            if !matches!(status, NativeCommandStatus::InProgress) {
                return Err(codex_error(
                    "Codex started a command outside its active state",
                ));
            }
            if correlation.active_commands.contains_key(&item_id) {
                return Err(codex_error("Codex reused an active command item identity"));
            }
            correlation.active_commands.insert(
                item_id.clone(),
                ActiveNativeCommand {
                    streamed_output: String::new(),
                },
            );
            Ok(vec![ProviderEvent::CommandStarted {
                activity_id: ProviderActivityId::new(item_id),
                command,
                cwd,
            }])
        }
        NativeNotification::CommandOutputDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(command) = correlation.active_commands.get_mut(&item_id) else {
                return Ok(Vec::new());
            };
            command.streamed_output.push_str(&delta);
            Ok(vec![ProviderEvent::CommandOutputDelta {
                activity_id: ProviderActivityId::new(item_id),
                content: delta,
            }])
        }
        NativeNotification::CommandCompleted {
            thread_id,
            turn_id,
            item_id,
            aggregated_output,
            exit_status,
            status,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(command) = correlation.active_commands.get(&item_id) else {
                return Err(codex_error(
                    "Codex completed a command before starting the Activity",
                ));
            };
            let remaining = match aggregated_output {
                Some(output) => output
                    .strip_prefix(&command.streamed_output)
                    .ok_or_else(|| {
                        codex_error(
                            "Codex completed a command with output that did not match its stream",
                        )
                    })?
                    .to_owned(),
                None => String::new(),
            };
            let status = match status {
                NativeCommandStatus::Completed => ProviderCommandStatus::Completed,
                NativeCommandStatus::Failed | NativeCommandStatus::Declined => {
                    ProviderCommandStatus::Failed
                }
                NativeCommandStatus::InProgress => {
                    return Err(codex_error(
                        "Codex completed a command while it was still active",
                    ));
                }
            };
            correlation.active_commands.remove(&item_id);
            let activity_id = ProviderActivityId::new(item_id);
            let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
            if !remaining.is_empty() {
                projected.push(ProviderEvent::CommandOutputDelta {
                    activity_id: activity_id.clone(),
                    content: remaining,
                });
            }
            projected.push(ProviderEvent::CommandCompleted {
                activity_id,
                status,
                exit_status,
            });
            Ok(projected)
        }
        NativeNotification::FileChangeStarted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            if !matches!(status, NativeFileChangeStatus::InProgress) {
                return Err(codex_error(
                    "Codex started file changes outside their active state",
                ));
            }
            if correlation.active_commands.contains_key(&item_id)
                || correlation.active_file_changes.contains_key(&item_id)
            {
                return Err(codex_error(
                    "Codex reused an active file-change item identity",
                ));
            }
            let changes = changes
                .into_iter()
                .map(FileChange::from)
                .collect::<Vec<_>>();
            correlation.active_file_changes.insert(
                item_id.clone(),
                ActiveNativeFileChange {
                    changes: changes.clone(),
                },
            );
            Ok(vec![ProviderEvent::FileChangeStarted {
                activity_id: ProviderActivityId::new(item_id),
                changes,
            }])
        }
        NativeNotification::FileChangeUpdated {
            thread_id,
            turn_id,
            item_id,
            changes,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(file_change) = correlation.active_file_changes.get_mut(&item_id) else {
                return Ok(Vec::new());
            };
            let changes = changes
                .into_iter()
                .map(FileChange::from)
                .collect::<Vec<_>>();
            file_change.changes.clone_from(&changes);
            Ok(vec![ProviderEvent::FileChangeUpdated {
                activity_id: ProviderActivityId::new(item_id),
                changes,
            }])
        }
        NativeNotification::FileChangeCompleted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => {
            if !is_active_native_turn(correlation, &thread_id, &turn_id) {
                return Ok(Vec::new());
            }
            let Some(file_change) = correlation.active_file_changes.get(&item_id) else {
                return Err(codex_error(
                    "Codex completed file changes before starting the Activity",
                ));
            };
            let changes = changes
                .into_iter()
                .map(FileChange::from)
                .collect::<Vec<_>>();
            let changes_changed = file_change.changes != changes;
            let status = match status {
                NativeFileChangeStatus::Completed => ProviderFileChangeStatus::Completed,
                NativeFileChangeStatus::Failed | NativeFileChangeStatus::Declined => {
                    ProviderFileChangeStatus::Failed
                }
                NativeFileChangeStatus::InProgress => {
                    return Err(codex_error(
                        "Codex completed file changes while they were still active",
                    ));
                }
            };
            correlation.active_file_changes.remove(&item_id);
            let activity_id = ProviderActivityId::new(item_id);
            let mut projected = Vec::with_capacity(if changes_changed { 2 } else { 1 });
            if changes_changed {
                projected.push(ProviderEvent::FileChangeUpdated {
                    activity_id: activity_id.clone(),
                    changes,
                });
            }
            projected.push(ProviderEvent::FileChangeCompleted {
                activity_id,
                status,
            });
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
            let selection_rejected = match (&outcome, &correlation.active_selection) {
                (NativeTurnOutcome::Failed { message, kind }, Some(selection)) => {
                    is_native_selection_rejection(message, kind, selection)
                }
                _ => false,
            };
            correlation.active_turn_id = None;
            correlation.active_selection = None;
            correlation.active_agent_message = None;
            correlation.active_commands.clear();
            correlation.active_file_changes.clear();
            Ok(vec![match outcome {
                NativeTurnOutcome::Completed => ProviderEvent::TurnCompleted,
                NativeTurnOutcome::Interrupted => ProviderEvent::TurnInterrupted,
                NativeTurnOutcome::Failed { message, .. } if selection_rejected => {
                    ProviderEvent::AgentSelectionRejected { message }
                }
                NativeTurnOutcome::Failed { message, .. } => ProviderEvent::TurnFailed { message },
            }])
        }
    }
}

fn apply_effective_select_option(
    selection: &mut AgentSelection,
    option_id: &str,
    field: NativeField<Option<String>>,
    clear: NativeClearMapping,
) -> Result<(), ProviderError> {
    let NativeField::Present(value) = field else {
        return Ok(());
    };
    let choice = match (value, clear) {
        (Some(choice), _) => choice,
        (None, NativeClearMapping::Select(choice)) => choice.to_owned(),
        (None, NativeClearMapping::RemoveOption) => {
            selection
                .options
                .retain(|option| option.id.as_str() != option_id);
            return Ok(());
        }
    };
    if choice.is_empty() {
        return Err(codex_error(format!(
            "Codex reported an empty effective value for Model Option `{option_id}`"
        )));
    }
    let value = ModelOptionValue::Select {
        choice: ModelOptionChoiceId::new(choice),
    };
    if let Some(option) = selection
        .options
        .iter_mut()
        .find(|option| option.id.as_str() == option_id)
    {
        option.value = value;
    } else {
        selection.options.push(ModelOptionSelection {
            id: ModelOptionId::new(option_id),
            value,
        });
    }
    Ok(())
}

enum NativeClearMapping {
    RemoveOption,
    Select(&'static str),
}

fn is_active_native_turn(correlation: &NativeCorrelation, thread_id: &str, turn_id: &str) -> bool {
    correlation.thread_id == thread_id && correlation.active_turn_id.as_deref() == Some(turn_id)
}

#[derive(Clone, Debug)]
struct ProcessRegistry {
    next_id: Arc<AtomicU64>,
    state: Arc<StdMutex<ProcessRegistryState>>,
}

#[derive(Debug)]
struct ProcessRegistryState {
    shutting_down: bool,
    processes: HashMap<u64, ProcessControl>,
}

impl ProcessRegistry {
    fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            state: Arc::new(StdMutex::new(ProcessRegistryState {
                shutting_down: false,
                processes: HashMap::new(),
            })),
        }
    }

    fn register(&self, process: ProcessControl) -> Result<u64, ProviderError> {
        let mut state = self
            .state
            .lock()
            .expect("Codex process registry lock is not poisoned");
        if state.shutting_down {
            return Err(codex_error("Codex runtime is shutting down"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.processes.insert(id, process);
        Ok(id)
    }

    fn remove(&self, id: u64) {
        self.state
            .lock()
            .expect("Codex process registry lock is not poisoned")
            .processes
            .remove(&id);
    }

    async fn shutdown(&self) -> Result<(), ProviderError> {
        let processes = {
            let mut state = self
                .state
                .lock()
                .expect("Codex process registry lock is not poisoned");
            state.shutting_down = true;
            state.processes.values().cloned().collect::<Vec<_>>()
        };
        for process in &processes {
            process.begin_shutdown();
        }
        let mut first_error = None;
        for process in processes {
            if let Err(error) = process.wait_until_stopped().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessControl {
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

impl ProcessControl {
    fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn wait_until_stopped(&self) -> Result<(), ProviderError> {
        let mut stopped = self.stopped.clone();
        if *stopped.borrow() {
            return Ok(());
        }
        let wait = async move {
            stopped
                .wait_for(|stopped| *stopped)
                .await
                .map(|_| ())
                .map_err(|_| {
                    codex_error("Codex app-server process supervisor stopped unexpectedly")
                })
        };
        timeout(
            PROCESS_EXIT_GRACE_PERIOD + PROCESS_KILL_TIMEOUT + Duration::from_millis(250),
            wait,
        )
        .await
        .map_err(|_| codex_error("Codex app-server did not stop within the shutdown deadline"))?
    }
}

struct ProcessGuard {
    control: ProcessControl,
}

impl ProcessGuard {
    fn begin_shutdown(&self) {
        self.control.begin_shutdown();
    }

    async fn wait_until_stopped(&self) -> Result<(), ProviderError> {
        self.control.wait_until_stopped().await
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct ChildSupervisor {
    child: Child,
    shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
    exit: watch::Sender<Option<ProviderError>>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<TransportState>,
    processes: ProcessRegistry,
    registration_id: u64,
    process_tree: ProcessTree,
}

async fn supervise_child(supervisor: ChildSupervisor) {
    let ChildSupervisor {
        mut child,
        mut shutdown,
        stopped,
        exit,
        writer,
        state,
        processes,
        registration_id,
        process_tree,
    } = supervisor;
    let status = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => {
            close_transport(
                &state,
                codex_error("Codex app-server transport closed during shutdown"),
            );
            match timeout(PROCESS_EXIT_GRACE_PERIOD, async {
                close_stdin(&writer).await;
                child.wait().await
            }).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = process_tree.terminate(&mut child);
                    match timeout(PROCESS_KILL_TIMEOUT, child.wait()).await {
                        Ok(status) => status,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "forced Codex termination did not complete before the deadline",
                        )),
                    }
                }
            }
        }
        status = child.wait() => status,
    };
    let _ = process_tree.terminate(&mut child);

    let message = match status {
        Ok(status) if status.success() => "Codex app-server exited unexpectedly".to_owned(),
        Ok(status) => format!("Codex app-server exited unexpectedly with {status}"),
        Err(error) => format!("could not wait for Codex app-server: {error}"),
    };
    let error = codex_error(message);
    exit.send_replace(Some(error.clone()));
    terminate_transport(&state, error);
    processes.remove(registration_id);
    process_tree.close();
    stopped.send_replace(true);
}

#[cfg(unix)]
struct ProcessTree {
    process_group_id: libc::pid_t,
}

#[cfg(unix)]
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    command.process_group(0);
    let child = command.spawn()?;
    let process_group_id = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Codex app-server had no process group ID",
            )
        })?;
    Ok((child, ProcessTree { process_group_id }))
}

#[cfg(unix)]
impl ProcessTree {
    fn terminate(&self, child: &mut Child) -> std::io::Result<()> {
        if unsafe { libc::killpg(self.process_group_id, libc::SIGKILL) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                if child.id().is_some() {
                    child.start_kill()?;
                }
                return Ok(());
            }
            let _ = child.start_kill();
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::{mem, os::windows::io::FromRawHandle, ptr};
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::CREATE_SUSPENDED,
    };

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process_handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }

    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        use std::os::windows::io::AsRawHandle;
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of!(limits).cast(),
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }

    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let process_handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("Codex app-server had no process handle"))?;
    let assigned = unsafe {
        use std::os::windows::io::AsRawHandle;
        AssignProcessToJobObject(job.as_raw_handle(), process_handle)
    };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.start_kill();
        return Err(error);
    }
    let resumed = unsafe { NtResumeProcess(process_handle) };
    if resumed < 0 {
        unsafe {
            use std::os::windows::io::AsRawHandle;
            TerminateJobObject(job.as_raw_handle(), 1);
        }
        return Err(std::io::Error::other(format!(
            "could not resume Codex app-server: NTSTATUS {resumed:#x}"
        )));
    }

    Ok((child, ProcessTree { job }))
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&self, _child: &mut Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    command.spawn().map(|child| (child, ProcessTree))
}

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn terminate(&self, child: &mut Child) -> std::io::Result<()> {
        child.start_kill()
    }
}

impl ProcessTree {
    /// Releases the containment handle before shutdown observers are notified.
    fn close(self) {}
}

type PendingResponse = oneshot::Sender<Result<Value, ProviderError>>;

struct TransportState {
    pending: StdMutex<HashMap<String, PendingResponse>>,
    events: mpsc::UnboundedSender<Result<NativeNotification, ProviderError>>,
    terminated: AtomicBool,
}

#[derive(Clone)]
struct JsonRpcTransport {
    writer: Arc<Mutex<Option<ChildStdin>>>,
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
            return Err(codex_error("Codex app-server transport has ended").mark_session_lost());
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
            ))
            .mark_session_lost()),
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

    async fn close(&self) {
        close_transport(
            &self.state,
            codex_error("Codex app-server transport closed during shutdown"),
        );
        close_stdin(&self.writer).await;
    }
}

async fn close_stdin(writer: &Arc<Mutex<Option<ChildStdin>>>) {
    let stdin = writer.lock().await.take();
    if let Some(mut stdin) = stdin {
        let _ = stdin.shutdown().await;
    }
}

async fn write_json_line<T: Serialize + ?Sized>(
    writer: &Arc<Mutex<Option<ChildStdin>>>,
    value: &T,
    operation: &str,
) -> Result<(), ProviderError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| codex_error(format!("could not encode Codex message: {error}")))?;
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    let writer = writer
        .as_mut()
        .ok_or_else(|| codex_error("Codex app-server transport has ended").mark_session_lost())?;
    writer.write_all(&bytes).await.map_err(|error| {
        codex_error(format!("could not {operation}: {error}")).mark_session_lost()
    })?;
    writer.flush().await.map_err(|error| {
        codex_error(format!("could not flush after {operation}: {error}")).mark_session_lost()
    })
}

async fn read_stdout(
    stdout: ChildStdout,
    writer: Arc<Mutex<Option<ChildStdin>>>,
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
    writer: &Arc<Mutex<Option<ChildStdin>>>,
    state: &Arc<TransportState>,
) -> Result<(), ProviderError> {
    if let Some(method) = message.method.as_deref() {
        if let Some(id) = message.id {
            if is_unsupported_interaction(method) {
                reject_server_request(
                    writer,
                    id,
                    UNSUPPORTED_INTERACTION_ERROR_CODE,
                    format!("Chidori does not support interactive request `{method}`"),
                )
                .await?;
                return Err(codex_error(format!(
                    "Codex app-server requested unsupported interaction `{method}`"
                )));
            }
            reject_server_request(
                writer,
                id,
                METHOD_NOT_FOUND_ERROR_CODE,
                format!("Chidori does not recognize server request `{method}`"),
            )
            .await?;
            return Ok(());
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
    writer: &Arc<Mutex<Option<ChildStdin>>>,
    id: RequestId,
    code: i64,
    message: String,
) -> Result<(), ProviderError> {
    write_json_line(
        writer,
        &ClientErrorResponse {
            id,
            error: ClientError { code, message },
        },
        "reject Codex app-server request",
    )
    .await
}

fn is_unsupported_interaction(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/tool/call"
    )
}

fn decode_notification(
    method: &str,
    params: Option<&Value>,
) -> Result<Option<NativeNotification>, ProviderError> {
    match method {
        "thread/settings/updated" => {
            let params: ThreadSettingsUpdatedParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::AgentSelectionChanged {
                thread_id: params.thread_id,
                model: params.thread_settings.model,
                effort: params.thread_settings.effort,
                service_tier: params.thread_settings.service_tier,
            }))
        }
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
                NativeItem::CommandExecution {
                    id,
                    command,
                    cwd,
                    status,
                    ..
                } => Ok(Some(NativeNotification::CommandStarted {
                    thread_id: params.thread_id,
                    turn_id: params.turn_id,
                    item_id: id,
                    command,
                    cwd,
                    status,
                })),
                NativeItem::FileChange {
                    id,
                    changes,
                    status,
                } => Ok(Some(NativeNotification::FileChangeStarted {
                    thread_id: params.thread_id,
                    turn_id: params.turn_id,
                    item_id: id,
                    changes,
                    status,
                })),
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
        "item/commandExecution/outputDelta" => {
            let params: AgentMessageDeltaParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::CommandOutputDelta {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                delta: params.delta,
            }))
        }
        "item/fileChange/patchUpdated" => {
            let params: FileChangeUpdatedParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::FileChangeUpdated {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                changes: params.changes,
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
                NativeItem::CommandExecution {
                    id,
                    aggregated_output,
                    exit_code,
                    status,
                    ..
                } => Ok(Some(NativeNotification::CommandCompleted {
                    thread_id: params.thread_id,
                    turn_id: params.turn_id,
                    item_id: id,
                    aggregated_output,
                    exit_status: exit_code,
                    status,
                })),
                NativeItem::FileChange {
                    id,
                    changes,
                    status,
                } => Ok(Some(NativeNotification::FileChangeCompleted {
                    thread_id: params.thread_id,
                    turn_id: params.turn_id,
                    item_id: id,
                    changes,
                    status,
                })),
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
                    kind: match params
                        .turn
                        .error
                        .as_ref()
                        .and_then(|error| error.codex_error_info.as_ref())
                    {
                        Some(NativeCodexErrorInfo::BadRequest) => {
                            NativeTurnFailureKind::BadRequest {
                                additional_details: params
                                    .turn
                                    .error
                                    .as_ref()
                                    .and_then(|error| error.additional_details.clone()),
                            }
                        }
                        Some(NativeCodexErrorInfo::Other) | None => NativeTurnFailureKind::Other,
                    },
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

fn codex_error_context(context: &str, error: ProviderError) -> ProviderError {
    let session_lost = error.is_session_lost();
    let selection_rejected = error.is_selection_rejected();
    let mut contextual = codex_error(format!("{context}: {error}"));
    if session_lost {
        contextual = contextual.mark_session_lost();
    }
    if selection_rejected {
        contextual = contextual.mark_selection_rejected();
    }
    contextual
}

fn terminate_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error.mark_session_lost(), true);
}

fn close_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error, false);
}

fn finish_transport(state: &TransportState, error: ProviderError, publish_error: bool) {
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
    if publish_error {
        let _ = state.events.send(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use crate::protocol::{
        AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, ProviderId,
    };

    use super::{
        CODEX_PATH_ENV, CodexRuntime, NativeTurnFailureKind, is_native_selection_rejection,
    };

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

    #[test]
    fn generic_failures_do_not_treat_incidental_choice_text_as_selection_rejection() {
        let selection = AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-fixture"),
            options: vec![ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low"),
                },
            }],
        };

        assert!(!is_native_selection_rejection(
            "Access denied because credits are low",
            &NativeTurnFailureKind::Other,
            &selection,
        ));
    }
}
