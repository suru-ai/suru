//! The Codex Provider runtime and the per-Session Turn lifecycle it hands back.
//!
//! [`CodexRuntime`] launches one app-server process per Suru Session, and [`CodexSession`] drives
//! that process's single native Turn slot: starting a Turn, steering it, interrupting it, and
//! stopping the process once the Session is done with it.

use std::{
    ffi::{OsStr, OsString},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};

use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID,
    codex_error, codex_error_context,
    projection::{NativeCorrelation, provider_events},
    transport::{CodexConnection, JsonRpcTransport},
    wire::{
        ModelListParams, NativeField, NativeModelList, TextInput, ThreadConnectionResult,
        ThreadResumeParams, ThreadStartParams, TurnInterruptParams, TurnStartParams,
        TurnStartResult, TurnSteerParams, TurnSteerResult, lower_reasoning_summary,
        lower_turn_options,
    },
};
use crate::{
    protocol::{
        AgentId, AgentIdentity, AgentSelection, EffectiveSettings, ModelDescriptor, ModelId,
        ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue, ProviderId,
        ReasoningSummaryDetail,
    },
    provider::{
        ProviderErrand, ProviderError, ProviderFuture, ProviderResumeState, ProviderRuntime,
        ProviderSession, ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput,
        ProviderTurnInput,
        harness::{ProcessGuard, ProcessRegistry},
        resolve_executable,
    },
};

const CODEX_PATH_ENV: &str = "SURU_CODEX_PATH";
const CODEX_EXECUTABLE_NAME: &str = "codex";
const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
const PENDING_TURN_START_GRACE_PERIOD: Duration = Duration::from_millis(250);

/// Launches one Codex app-server process for each Suru Session.
#[derive(Clone, Debug)]
pub struct CodexRuntime {
    executable: OsString,
    processes: ProcessRegistry,
    interrupt_request_timeout: Duration,
    shutdown_interrupt_timeout: Duration,
    /// The Reasoning summary detail Turns ask for, shared with every Session
    /// this runtime started so the value each Turn sends is the one in force
    /// when it starts rather than the one its Session opened with.
    reasoning_summary: Arc<StdMutex<ReasoningSummaryDetail>>,
}

#[derive(Deserialize, Serialize)]
struct CodexResumeState {
    thread_id: CodexThreadId,
}

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct CodexThreadId(String);

impl CodexThreadId {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl CodexRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(super::CODEX_HARNESS_NAME),
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
            shutdown_interrupt_timeout: SHUTDOWN_INTERRUPT_REQUEST_TIMEOUT,
            reasoning_summary: Arc::new(StdMutex::new(ReasoningSummaryDetail::default())),
        }
    }

    /// Bounds how long a Session shutdown waits for Codex to acknowledge the
    /// courtesy interrupt before forcing the process down; injectable so tests
    /// with unresponsive fixtures do not wait out the default.
    pub fn with_shutdown_interrupt_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_interrupt_timeout = timeout;
        self
    }

    /// Bounds how long a stopping Codex process may exit gracefully before it
    /// is forced down; injectable so tests with fixtures that ignore stdin
    /// closure do not wait out the default.
    pub fn with_process_exit_grace(mut self, exit_grace: Duration) -> Self {
        self.processes.set_exit_grace(exit_grace);
        self
    }

    /// Bounds how long an interrupt RPC waits for Codex to acknowledge; injectable
    /// so tests can exercise the timeout without waiting out the default.
    pub fn with_interrupt_request_timeout(mut self, timeout: Duration) -> Self {
        self.interrupt_request_timeout = timeout;
        self
    }

    pub fn from_environment() -> Self {
        Self::new(resolve_executable(CODEX_PATH_ENV, CODEX_EXECUTABLE_NAME))
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

    fn display_name(&self) -> &str {
        "Codex"
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
        let context = SessionContext {
            timeouts: SessionTimeouts {
                interrupt_request: self.interrupt_request_timeout,
                shutdown_interrupt: self.shutdown_interrupt_timeout,
            },
            reasoning_summary: self.reasoning_summary.clone(),
        };
        Box::pin(async move { start_codex_session(executable, request, processes, context).await })
    }

    /// Codex fulfils an Errand through `codex exec`, its own one-shot mode:
    /// a separate, short-lived process from the app-server every Session is
    /// driven over, which persists nothing and is never resumed.
    fn run_errand(&self, errand: ProviderErrand) -> ProviderFuture<'_, serde_json::Value> {
        let executable = self.executable.clone();
        Box::pin(async move { super::errand::run(executable, errand).await })
    }

    fn errand_selection(&self) -> Option<AgentSelection> {
        Some(super::errand::declared_errand_selection())
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.processes.shutdown().await })
    }

    fn apply_settings(&self, settings: &EffectiveSettings) {
        *self
            .reasoning_summary
            .lock()
            .expect("Codex Reasoning summary Setting lock is not poisoned") =
            settings.provider.codex.reasoning_summary;
    }
}

async fn discover_codex_models(
    executable: OsString,
    processes: ProcessRegistry,
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let CodexConnection {
        transport,
        notifications: _notifications,
        process,
    } = JsonRpcTransport::launch(&executable, processes).await?;
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
        let next_cursor = page.next_cursor.clone();
        models.extend(page.visible_models());
        match next_cursor {
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

/// The interruption-related timeouts a [`CodexRuntime`] hands each Session.
#[derive(Clone, Copy, Debug)]
struct SessionTimeouts {
    interrupt_request: Duration,
    shutdown_interrupt: Duration,
}

/// What a Session carries over from the [`CodexRuntime`] that started it: the
/// timeouts it was built with, and the handle on the Server Settings it reads
/// each Turn from. One value rather than two parameters, because every Server
/// Setting Codex grows lands here beside the first.
#[derive(Clone, Debug)]
struct SessionContext {
    timeouts: SessionTimeouts,
    reasoning_summary: Arc<StdMutex<ReasoningSummaryDetail>>,
}

async fn start_codex_session(
    executable: OsString,
    request: ProviderSessionRequest,
    processes: ProcessRegistry,
    context: SessionContext,
) -> Result<ProviderSessionConnection, ProviderError> {
    let connection = JsonRpcTransport::launch(&executable, processes).await?;
    start_codex_thread(connection, request, context).await
}

async fn start_codex_thread(
    connection: CodexConnection,
    request: ProviderSessionRequest,
    context: SessionContext,
) -> Result<ProviderSessionConnection, ProviderError> {
    let CodexConnection {
        transport,
        notifications,
        process,
    } = connection;
    let cwd = request
        .workspace
        .to_str()
        .ok_or_else(|| codex_error("Workspace path cannot be represented for Codex app-server"))?;
    let known_thread_id = request
        .resume_state
        .map(ProviderResumeState::into_payload)
        .map(serde_json::from_value::<CodexResumeState>)
        .transpose()
        .map_err(|error| codex_error(format!("Codex Resume State is invalid: {error}")))?
        .map(|state| state.thread_id);
    let (method, result) = if let Some(thread_id) = known_thread_id.as_ref() {
        let result = transport
            .request(
                "thread/resume",
                &ThreadResumeParams {
                    thread_id: thread_id.as_str(),
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
    let resume_state = ProviderResumeState::new(
        serde_json::to_value(CodexResumeState {
            thread_id: CodexThreadId(started.thread.id.clone()),
        })
        .expect("Codex Resume State serialization is infallible"),
    );

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

    let correlation = Arc::new(StdMutex::new(NativeCorrelation::new(
        started.thread.id.clone(),
    )));
    let turn_start_changed = Arc::new(Notify::new());
    let session = Arc::new(CodexSession {
        thread_id: started.thread.id,
        context,
        transport,
        correlation: correlation.clone(),
        turn_start_changed,
        process: process.clone(),
        shutdown_started: AtomicBool::new(false),
    });
    let events = provider_events(notifications, process, correlation);
    Ok(ProviderSessionConnection::new(
        AgentIdentity {
            agent: AgentId::new("codex"),
            selection: AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new(started.model),
                options: initial_options,
            },
        },
        Some(resume_state),
        session,
        events,
    ))
}

struct CodexSession {
    thread_id: String,
    context: SessionContext,
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
                correlation.begin_turn_start()?;
            }
            // Read here rather than held from Session startup, so the Turn
            // about to run asks for the detail the Setting names now.
            let summary = lower_reasoning_summary(
                *self
                    .context
                    .reasoning_summary
                    .lock()
                    .expect("Codex Reasoning summary Setting lock is not poisoned"),
            );
            let task = tokio::spawn(start_native_turn(
                self.thread_id.clone(),
                input.prompt,
                input.selection,
                summary,
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
                .active_turn_id()
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
                .active_turn_id()
                .ok_or_else(|| codex_error("Codex has no active Turn to interrupt"))?;
            self.transport
                .request_with_timeout(
                    "turn/interrupt",
                    &TurnInterruptParams {
                        thread_id: &self.thread_id,
                        turn_id: &turn_id,
                    },
                    self.context.timeouts.interrupt_request,
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
                (correlation.active_turn_id(), correlation.is_turn_pending())
            };
            if active_turn_id.is_none() && turn_starting {
                let _ = timeout(PENDING_TURN_START_GRACE_PERIOD, turn_start_changed).await;
                active_turn_id = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned")
                    .active_turn_id();
            }
            if let Some(turn_id) = active_turn_id {
                let _ = timeout(
                    self.context.timeouts.shutdown_interrupt,
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
    summary: &'static str,
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
                    summary,
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

    let result = correlation
        .lock()
        .expect("Codex native correlation lock is not poisoned")
        .finish_turn_start(started, selection);
    turn_start_changed.notify_one();
    result
}
