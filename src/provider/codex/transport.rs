//! The JSON-RPC transport Suru speaks over a Codex app-server's stdio.
//!
//! [`JsonRpcTransport::launch`] is the only way to obtain one: it spawns a supervised app-server,
//! completes the handshake, and returns the request channel alongside the notification stream.
//! Requests correlate by ID, notifications decode into [`NativeNotification`], and every path that
//! ends the connection fails the in-flight requests exactly once.

use std::{
    collections::HashMap,
    ffi::OsStr,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, mpsc, oneshot, watch},
    time::{Duration, timeout},
};

use super::{
    approval::{NativeApprovalTurn, NativeInterruptTarget},
    codex_error, codex_error_context, concise_remote_message,
    wire::{
        ClientError, ClientErrorResponse, ClientInfo, ClientNotification, ClientRequest,
        CompletedNativeAgentMessage, FileChangeUpdatedParams, IncomingMessage,
        InitializeCapabilities, InitializeParams, ItemDeltaParams, ItemNotificationParams,
        NativeCodexErrorInfo, NativeItem, NativeNotification, NativeTurnFailureKind,
        NativeTurnOutcome, NativeTurnStatus, ReasoningSectionBreakParams,
        ReasoningSummaryDeltaParams, RequestId, ThreadSettingsUpdatedParams,
        ThreadTokenUsageParams, TurnCompletedParams, TurnStartedParams,
    },
};
use crate::provider::{
    ProviderError,
    harness::{
        HarnessLink, HarnessSpec, ProcessGuard, ProcessRegistry, ProcessStdio,
        spawn_harness_process, supervise_harness_process,
    },
    version::SuggestedCliVersion,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const UNSUPPORTED_INTERACTION_ERROR_CODE: i64 = -32000;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const CODEX_SUGGESTED_VERSION: SuggestedCliVersion =
    SuggestedCliVersion::new("Codex CLI", 0, 150, 1);

type PendingResponse = oneshot::Sender<Result<Value, ProviderError>>;

struct TransportState {
    pending: StdMutex<HashMap<String, PendingResponse>>,
    questionnaires: super::questionnaire::CodexQuestionnaires,
    approvals: super::approval::CodexApprovals,
    decision_settlements: NativeDecisionSettlements,
    events: mpsc::UnboundedSender<Result<NativeNotification, ProviderError>>,
    terminated: AtomicBool,
}

#[derive(Clone, Default)]
struct NativeDecisionSettlements(Arc<StdMutex<HashMap<NativeApprovalTurn, watch::Sender<usize>>>>);

impl NativeDecisionSettlements {
    fn begin(&self, turn: NativeApprovalTurn) -> NativeDecisionSettlement {
        let mut turns = self
            .0
            .lock()
            .expect("Codex Decision settlement lock is not poisoned");
        let pending = turns
            .entry(turn.clone())
            .or_insert_with(|| watch::channel(0).0);
        let next = (*pending.borrow()).saturating_add(1);
        pending.send_replace(next);
        NativeDecisionSettlement {
            settlements: self.clone(),
            turn: Some(turn),
        }
    }

    fn finish(&self, turn: &NativeApprovalTurn) {
        let mut turns = self
            .0
            .lock()
            .expect("Codex Decision settlement lock is not poisoned");
        let remove = turns.get(turn).is_some_and(|pending| {
            let next = (*pending.borrow()).saturating_sub(1);
            pending.send_replace(next);
            next == 0
        });
        if remove {
            turns.remove(turn);
        }
    }

    async fn wait(&self, turn: &NativeApprovalTurn) {
        let Some(mut pending) = self
            .0
            .lock()
            .expect("Codex Decision settlement lock is not poisoned")
            .get(turn)
            .map(watch::Sender::subscribe)
        else {
            return;
        };
        while *pending.borrow_and_update() > 0 && pending.changed().await.is_ok() {}
    }
}

pub(super) struct NativeDecisionSettlement {
    settlements: NativeDecisionSettlements,
    turn: Option<NativeApprovalTurn>,
}

impl Drop for NativeDecisionSettlement {
    fn drop(&mut self) {
        if let Some(turn) = self.turn.take() {
            self.settlements.finish(&turn);
        }
    }
}

pub(super) struct NativeDecisionDelivery {
    pub(super) settlement: NativeDecisionSettlement,
    pub(super) interrupt: Option<NativeInterruptTarget>,
}

/// An initialized connection to one supervised Codex app-server.
pub(super) struct CodexConnection {
    pub(super) transport: JsonRpcTransport,
    pub(super) notifications: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    pub(super) process: Arc<ProcessGuard>,
    /// A non-blocking compatibility condition learned from the handshake.
    /// Sessions do not need it; Model discovery carries it to Provider views.
    pub(super) warning: Option<String>,
}

fn codex_version_warning(initialize: &Value) -> Option<String> {
    let user_agent = initialize.get("userAgent")?.as_str()?;
    let reported = user_agent.split_whitespace().next()?.rsplit_once('/')?.1;
    CODEX_SUGGESTED_VERSION.warning_for(reported).ok().flatten()
}

#[derive(Clone)]
pub(super) struct JsonRpcTransport {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<TransportState>,
    next_id: Arc<AtomicI64>,
    _process: Arc<ProcessGuard>,
}

impl JsonRpcTransport {
    /// Launches a supervised app-server and completes the protocol handshake with it.
    pub(super) async fn launch(
        executable: &OsStr,
        processes: ProcessRegistry,
    ) -> Result<CodexConnection, ProviderError> {
        let spec = HarnessSpec {
            executable: executable.to_owned(),
            args: vec!["app-server".into()],
            name: super::CODEX_HARNESS_NAME.to_owned(),
            cwd: None,
        };
        let (process, ProcessStdio { stdin, stdout }) = spawn_harness_process(&spec)?;
        let (events, notifications) = mpsc::unbounded_channel();
        let state = Arc::new(TransportState {
            pending: StdMutex::new(HashMap::new()),
            questionnaires: super::questionnaire::CodexQuestionnaires::default(),
            approvals: super::approval::CodexApprovals::default(),
            decision_settlements: NativeDecisionSettlements::default(),
            events,
            terminated: AtomicBool::new(false),
        });
        let writer = Arc::new(Mutex::new(Some(stdin)));
        let link = TransportLink {
            writer: writer.clone(),
            state: state.clone(),
        };
        let (process, exit) = supervise_harness_process(process, processes, link).await?;
        tokio::spawn(read_stdout(stdout, writer.clone(), state.clone(), exit));

        let transport = Self {
            writer,
            state,
            next_id: Arc::new(AtomicI64::new(1)),
            _process: process.clone(),
        };
        let initialize = transport
            .request(
                "initialize",
                &InitializeParams {
                    client_info: ClientInfo {
                        name: "suru",
                        title: "Suru",
                        version: env!("CARGO_PKG_VERSION"),
                    },
                    capabilities: InitializeCapabilities {
                        experimental_api: false,
                    },
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex initialization failed", error))?;
        let warning = codex_version_warning(&initialize);
        transport
            .notify("initialized")
            .await
            .map_err(|error| codex_error_context("Codex initialization failed", error))?;

        Ok(CodexConnection {
            transport,
            notifications,
            process,
            warning,
        })
    }

    pub(super) async fn request<T: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<Value, ProviderError> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    pub(super) async fn request_with_timeout<T: Serialize + ?Sized>(
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

    pub(super) fn questionnaires(&self) -> super::questionnaire::CodexQuestionnaires {
        self.state.questionnaires.clone()
    }

    pub(super) fn approvals(&self) -> super::approval::CodexApprovals {
        self.state.approvals.clone()
    }

    pub(super) async fn submit_questionnaire(
        &self,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> Result<(), ProviderError> {
        let (id, result) = self.state.questionnaires.take_response(id, submission)?;
        write_json_line(
            &self.writer,
            &super::wire::ClientResponse { id, result },
            "answer Codex user-input request",
        )
        .await
    }

    pub(super) async fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> Result<NativeDecisionDelivery, ProviderError> {
        let native = self.state.approvals.take_decision(id, decision)?;
        let settlement = self.state.decision_settlements.begin(native.turn.clone());
        write_json_line(
            &self.writer,
            &super::wire::ClientResponse {
                id: native.request_id,
                result: native.result,
            },
            "answer Codex Approval request",
        )
        .await?;
        Ok(NativeDecisionDelivery {
            settlement,
            interrupt: native.interrupt,
        })
    }

    pub(super) async fn close(&self) {
        close_transport(
            &self.state,
            codex_error("Codex app-server transport closed during shutdown"),
        );
        close_stdin(&self.writer).await;
    }
}

/// The process supervisor's handle to the transport running over the process it owns.
///
/// The supervisor decides when the connection ends; this is everything it needs to say so.
pub(super) struct TransportLink {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<TransportState>,
}

impl HarnessLink for TransportLink {
    fn close(&self) {
        close_transport(
            &self.state,
            codex_error("Codex app-server transport closed during shutdown"),
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
            Err(_) => {
                terminate_transport(&state, codex_error("Codex app-server sent malformed JSON"));
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
    mut message: IncomingMessage,
    writer: &Arc<Mutex<Option<ChildStdin>>>,
    state: &Arc<TransportState>,
) -> Result<(), ProviderError> {
    if let Some(error) = &mut message.error {
        error.message = state.questionnaires.redact_text(&error.message);
    }
    if let Some(method) = message.method.as_deref() {
        let display_method = state.questionnaires.redact_text(method);
        if let Some(id) = message.id {
            if method == "item/tool/requestUserInput" {
                let params = decode_notification_params(method, message.params.as_ref())?;
                for event in state
                    .questionnaires
                    .redact_notification(NativeNotification::QuestionnaireRequested { id, params })
                {
                    let _ = state.events.send(Ok(event));
                }
                return Ok(());
            }
            let approval = match method {
                "item/commandExecution/requestApproval" => {
                    Some(NativeNotification::CommandApprovalRequested {
                        id: id.clone(),
                        params: decode_notification_params(method, message.params.as_ref())?,
                    })
                }
                "item/fileChange/requestApproval" => {
                    Some(NativeNotification::FileChangeApprovalRequested {
                        id: id.clone(),
                        params: decode_notification_params(method, message.params.as_ref())?,
                    })
                }
                "item/permissions/requestApproval" => {
                    let params = decode_notification_params::<
                        super::wire::PermissionsApprovalParams,
                    >(method, message.params.as_ref())?;
                    let native_permissions = params.permissions.clone();
                    Some(NativeNotification::PermissionsApprovalRequested {
                        id: id.clone(),
                        params,
                        native_permissions,
                    })
                }
                _ => None,
            };
            if let Some(approval) = approval {
                for event in state.questionnaires.redact_notification(approval) {
                    let _ = state.events.send(Ok(event));
                }
                return Ok(());
            }
            if is_unsupported_interaction(method) {
                reject_server_request(
                    writer,
                    id,
                    UNSUPPORTED_INTERACTION_ERROR_CODE,
                    format!("Suru does not support interactive request `{display_method}`"),
                )
                .await?;
                tracing::warn!(
                    method = %display_method,
                    "Codex app-server requested an unsupported interaction"
                );
                return Ok(());
            }
            reject_server_request(
                writer,
                id,
                METHOD_NOT_FOUND_ERROR_CODE,
                format!("Suru does not recognize server request `{display_method}`"),
            )
            .await?;
            return Ok(());
        }
        if let Some(event) = decode_notification(method, message.params.as_ref())? {
            if let NativeNotification::TurnCompleted {
                thread_id, turn_id, ..
            } = &event
            {
                state
                    .decision_settlements
                    .wait(&NativeApprovalTurn {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                    })
                    .await;
            }
            for event in state.questionnaires.redact_notification(event) {
                let _ = state.events.send(Ok(event));
            }
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
    matches!(method, "mcpServer/elicitation/request" | "item/tool/call")
}

fn decode_notification(
    method: &str,
    params: Option<&Value>,
) -> Result<Option<NativeNotification>, ProviderError> {
    match method {
        "serverRequest/resolved" => {
            let params: super::wire::ServerRequestResolvedParams =
                decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::QuestionnaireResolved {
                thread_id: params.thread_id,
                request_id: params.request_id,
            }))
        }
        "skills/changed" => Ok(Some(NativeNotification::SkillsChanged)),
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
                NativeItem::Reasoning { id, .. } => {
                    Ok(Some(NativeNotification::ReasoningStarted {
                        thread_id: params.thread_id,
                        turn_id: params.turn_id,
                        item_id: id,
                    }))
                }
                // Collab calls and subagent activity read whole from the
                // completed item, so their starts carry nothing further.
                NativeItem::CollabAgentToolCall { .. }
                | NativeItem::SubAgentActivity { .. }
                | NativeItem::Unknown => Ok(None),
            }
        }
        "item/agentMessage/delta" => {
            let params: ItemDeltaParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::AgentMessageDelta {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                delta: params.delta,
            }))
        }
        "item/commandExecution/outputDelta" => {
            let params: ItemDeltaParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::CommandOutputDelta {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                delta: params.delta,
            }))
        }
        "item/reasoning/summaryTextDelta" => {
            let params: ReasoningSummaryDeltaParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::ReasoningDelta {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                delta: params.delta,
                summary_index: params.summary_index,
            }))
        }
        "item/reasoning/summaryPartAdded" => {
            let params: ReasoningSectionBreakParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::ReasoningSectionBreak {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                item_id: params.item_id,
                summary_index: params.summary_index,
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
                NativeItem::Reasoning { id, summary } => {
                    Ok(Some(NativeNotification::ReasoningCompleted {
                        thread_id: params.thread_id,
                        turn_id: params.turn_id,
                        item_id: id,
                        summary,
                    }))
                }
                NativeItem::CollabAgentToolCall {
                    tool,
                    status,
                    receiver_thread_ids,
                    prompt,
                    agents_states,
                } => Ok(Some(NativeNotification::CollabCallCompleted {
                    thread_id: params.thread_id,
                    tool,
                    status,
                    receiver_thread_ids,
                    prompt,
                    agents_states,
                })),
                NativeItem::SubAgentActivity {
                    kind,
                    agent_thread_id,
                    agent_path,
                } => Ok(Some(NativeNotification::SubagentActivity {
                    thread_id: params.thread_id,
                    kind,
                    agent_thread_id,
                    agent_path,
                })),
                NativeItem::Unknown => Ok(None),
            }
        }
        "thread/tokenUsage/updated" => {
            let params: ThreadTokenUsageParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::TokenUsage {
                thread_id: params.thread_id,
                turn_id: params.turn_id,
                context_fill: params.token_usage.context_fill(),
                total: params.token_usage.into_cumulative(),
            }))
        }
        "turn/started" => {
            let params: TurnStartedParams = decode_notification_params(method, params)?;
            Ok(Some(NativeNotification::TurnStarted {
                thread_id: params.thread_id,
                turn_id: params.turn.id,
            }))
        }
        "turn/completed" => {
            let params: TurnCompletedParams = decode_notification_params(method, params)?;
            let final_agent_message = params.turn.items.into_iter().rev().find_map(|item| {
                let NativeItem::AgentMessage { id, text } = item else {
                    return None;
                };
                Some(CompletedNativeAgentMessage { item_id: id, text })
            });
            let outcome = match params.turn.status {
                NativeTurnStatus::Completed => NativeTurnOutcome::Completed,
                NativeTurnStatus::Interrupted => NativeTurnOutcome::Interrupted,
                NativeTurnStatus::Failed => NativeTurnOutcome::Failed {
                    message: params
                        .turn
                        .error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .unwrap_or_else(|| "Codex Turn failed".into()),
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
                final_agent_message,
            }))
        }
        _ => Ok(None),
    }
}

fn decode_notification_params<T: for<'de> serde::Deserialize<'de>>(
    method: &str,
    params: Option<&Value>,
) -> Result<T, ProviderError> {
    let params = params.ok_or_else(|| {
        codex_error(format!(
            "Codex app-server notification `{method}` omitted params"
        ))
    })?;
    serde_json::from_value(params.clone()).map_err(|_| {
        codex_error(format!(
            "Codex app-server notification `{method}` had invalid params"
        ))
    })
}

fn terminate_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error.mark_session_lost(), true);
}

fn close_transport(state: &TransportState, error: ProviderError) {
    finish_transport(state, error, false);
}

fn finish_transport(state: &TransportState, error: ProviderError, publish_error: bool) {
    state.questionnaires.clear();
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
    use serde_json::json;

    use super::codex_version_warning;

    fn warning_for(version: &str) -> Option<String> {
        codex_version_warning(&json!({
            "userAgent": format!("suru/{version} (Linux 6; x86_64) codex_cli_rs/0.1")
        }))
    }

    #[test]
    fn codex_versions_below_0_150_1_warn_about_compatibility() {
        for version in ["0.149.0", "0.150.0", "0.150.1-alpha.1"] {
            let warning = warning_for(version)
                .unwrap_or_else(|| panic!("Codex CLI {version} should carry the warning"));
            assert!(warning.contains(version));
            assert!(warning.contains("0.150.1 or newer"));
            assert!(warning.contains("may have compatibility issues"));
        }
    }

    #[test]
    fn codex_versions_at_or_above_0_150_1_carry_no_warning() {
        for version in ["0.150.1", "0.150.1+build.7", "0.151.0", "1.0.0"] {
            assert_eq!(warning_for(version), None, "Codex CLI {version}");
        }
    }

    #[test]
    fn a_missing_or_unreadable_codex_version_invents_no_warning() {
        for initialize in [
            json!({}),
            json!({"userAgent": "codex"}),
            json!({"userAgent": "suru/not-a-version"}),
        ] {
            assert_eq!(codex_version_warning(&initialize), None);
        }
    }
}
