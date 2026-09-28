//! The Codex Provider runtime and the per-Session Turn lifecycle it hands back.
//!
//! [`CodexRuntime`] launches one app-server process per Suru Session, and [`CodexSession`] drives
//! that process's single native Turn slot: starting a Turn, steering it, interrupting it, and
//! stopping the process once the Session is done with it. A Session handed the Broker hands it on
//! to its thread through the per-thread `config` map, on `thread/start` and on every
//! `thread/resume` a relaunch sends, with the token that launch was handed.

use std::{
    ffi::{OsStr, OsString},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::{Notify, watch},
    time::{Duration, timeout},
};

use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID,
    codex_error, codex_error_context,
    projection::{ChildThreadAttachment, NativeCorrelation, provider_events},
    skills::CodexSkills,
    transport::{CodexConnection, JsonRpcTransport},
    wire::{
        CodexPosture, ModelListParams, NativeField, NativeModelList, SubagentTurnStartParams,
        ThreadConnectionResult, ThreadResumeParams, ThreadStartParams, TurnInterruptParams,
        TurnStartParams, TurnStartResult, TurnSteerParams, TurnSteerResult,
        broker_developer_instructions, broker_thread_config, lower_reasoning_summary,
        lower_turn_options, user_input_text,
    },
};
use crate::{
    pricing::PricingSource,
    protocol::{
        AgentId, AgentIdentity, AgentSelection, EffectiveSettings, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionSelection, ModelOptionValue, ProviderId, ReasoningSummaryDetail,
        SkillCatalog,
    },
    provider::{
        ProviderDecisionDelivery, ProviderErrand, ProviderError, ProviderFuture, ProviderInput,
        ProviderModelDiscovery, ProviderResumeState, ProviderRuntime, ProviderSession,
        ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput, ProviderSubagentId,
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
    posture: Arc<StdMutex<CodexPosture>>,
    skills: CodexSkills,
    skill_catalog_invalidations: watch::Sender<u64>,
    /// The rate table every Session this runtime starts estimates its Costs
    /// from. Absent until the server hands one over, which leaves Costs absent
    /// and tokens intact.
    pricing: Option<Arc<PricingSource>>,
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
        let (skill_catalog_invalidations, _) = watch::channel(0);
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(super::CODEX_HARNESS_NAME),
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
            shutdown_interrupt_timeout: SHUTDOWN_INTERRUPT_REQUEST_TIMEOUT,
            reasoning_summary: Arc::new(StdMutex::new(ReasoningSummaryDetail::default())),
            posture: Arc::new(StdMutex::new(CodexPosture::default())),
            skills: CodexSkills::default(),
            skill_catalog_invalidations,
            pricing: None,
        }
    }

    /// Hands this runtime the models.dev rate lookup its Sessions price Turns
    /// from. Codex states no dollar figure of its own, so this is what decides
    /// whether a Codex Turn carries an Estimated Cost at all; injectable so
    /// tests point it at a fixture rather than the network.
    pub fn with_pricing_source(mut self, pricing: Arc<PricingSource>) -> Self {
        self.pricing = Some(pricing);
        self
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

    fn nerd_font_icon(&self) -> Option<char> {
        // Nerd Fonts `nf-cod-openai` (Codicons OpenAI).
        Some('\u{ec81}')
    }

    // A collab child is a thread of its own, and `turn/interrupt` addresses
    // any thread — which is exactly a per-Subagent stop.
    fn supports_subagent_stop(&self) -> bool {
        true
    }

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        Box::pin(async move { discover_codex_models(executable, processes).await })
    }

    fn skill_catalog(
        &self,
        execution_directory: &std::path::Path,
    ) -> ProviderFuture<'_, SkillCatalog> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let skills = self.skills.clone();
        let execution_directory = execution_directory.to_owned();
        Box::pin(async move {
            skills
                .discover(&executable, processes, &execution_directory, false)
                .await
        })
    }

    fn refresh_skill_catalog(
        &self,
        execution_directory: &std::path::Path,
    ) -> ProviderFuture<'_, SkillCatalog> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let skills = self.skills.clone();
        let execution_directory = execution_directory.to_owned();
        Box::pin(async move {
            skills
                .discover(&executable, processes, &execution_directory, true)
                .await
        })
    }

    fn subscribe_skill_catalog_invalidations(&self) -> Option<watch::Receiver<u64>> {
        Some(self.skill_catalog_invalidations.subscribe())
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
            posture: Arc::new(StdMutex::new(match request.approval_posture.as_ref() {
                Some(crate::protocol::ApprovalPosture::Codex {
                    approval_policy,
                    sandbox_mode,
                }) => CodexPosture {
                    approval_policy: *approval_policy,
                    sandbox_mode: *sandbox_mode,
                },
                _ => *self
                    .posture
                    .lock()
                    .expect("Codex posture Setting lock is not poisoned"),
            })),
            skills: self.skills.clone(),
            skill_catalog_invalidations: self.skill_catalog_invalidations.clone(),
            execution_directory: request.execution_directory.clone(),
            pricing: self.pricing.clone(),
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
        *self
            .posture
            .lock()
            .expect("Codex posture Setting lock is not poisoned") = CodexPosture {
            approval_policy: settings.provider.codex.approval_policy,
            sandbox_mode: settings.provider.codex.sandbox_mode,
        };
    }
}

async fn discover_codex_models(
    executable: OsString,
    processes: ProcessRegistry,
) -> Result<ProviderModelDiscovery, ProviderError> {
    let CodexConnection {
        transport,
        notifications: _notifications,
        process,
        warning,
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
        let page: NativeModelList = serde_json::from_value(result)
            .map_err(|_| codex_error("Codex returned an invalid model/list response"))?;
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
    Ok(match warning {
        Some(warning) => ProviderModelDiscovery::new(models).with_warning(warning),
        None => ProviderModelDiscovery::new(models),
    })
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
    posture: Arc<StdMutex<CodexPosture>>,
    skills: CodexSkills,
    skill_catalog_invalidations: watch::Sender<u64>,
    execution_directory: std::path::PathBuf,
    pricing: Option<Arc<PricingSource>>,
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
        warning: _,
    } = connection;
    let cwd = request
        .execution_directory
        .to_str()
        .ok_or_else(|| codex_error("Workspace path cannot be represented for Codex app-server"))?;
    let known_thread_id = request
        .resume_state
        .map(ProviderResumeState::into_payload)
        .map(serde_json::from_value::<CodexResumeState>)
        .transpose()
        .map_err(|error| codex_error(format!("Codex Resume State is invalid: {error}")))?
        .map(|state| state.thread_id);
    // The Broker goes on the thread's start and on every resume alike, carrying the token this
    // launch was handed, because Codex keeps nothing of an MCP server with the thread; and so does
    // its note, which a resumed thread reads from the configuration it is resumed under.
    let config = request.broker.as_ref().map(broker_thread_config);
    let note = request.broker.is_some().then(broker_developer_instructions);
    let (method, result) = if let Some(thread_id) = known_thread_id.as_ref() {
        let posture = *context
            .posture
            .lock()
            .expect("Codex Session posture lock is not poisoned");
        let result = transport
            .request(
                "thread/resume",
                &ThreadResumeParams {
                    thread_id: thread_id.as_str(),
                    cwd,
                    approval_policy: posture.approval_policy(),
                    sandbox: posture.sandbox(),
                    config: config.as_ref(),
                    developer_instructions: note.as_deref(),
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Session resume failed", error))?;
        ("thread/resume", result)
    } else {
        let posture = *context
            .posture
            .lock()
            .expect("Codex Session posture lock is not poisoned");
        let result = transport
            .request(
                "thread/start",
                &ThreadStartParams {
                    cwd,
                    approval_policy: posture.approval_policy(),
                    sandbox: posture.sandbox(),
                    ephemeral: false,
                    config: config.as_ref(),
                    developer_instructions: note.as_deref(),
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Session startup failed", error))?;
        ("thread/start", result)
    };
    let started: ThreadConnectionResult = serde_json::from_value(result)
        .map_err(|_| codex_error(format!("Codex returned an invalid {method} response")))?;
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
        ModelId::new(started.model.clone()),
        transport.questionnaires(),
        transport.approvals(),
    )));
    let turn_start_changed = Arc::new(Notify::new());
    let skill_catalog_invalidations = context.skill_catalog_invalidations.clone();
    // Refresh belongs to the Session lifetime, outside the event pump.
    let pricing = context.pricing.clone();
    let pricing_refresh = pricing.as_ref().map(PricingSource::keep_fresh);
    let attachment = ChildThreadAttachment {
        transport: transport.clone(),
        cwd: cwd.to_owned(),
        posture: context.posture.clone(),
    };
    let session = Arc::new(CodexSession {
        thread_id: started.thread.id,
        context,
        transport,
        child_attachment: attachment.clone(),
        correlation: correlation.clone(),
        turn_start_changed,
        process: process.clone(),
        shutdown_started: AtomicBool::new(false),
        pricing_refresh,
    });
    let events = provider_events(
        notifications,
        process,
        correlation,
        skill_catalog_invalidations,
        attachment,
        pricing,
    );
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
    /// Attaches a child thread before input is handed to it (see
    /// [`ProviderSession::deliver_to_subagent`]).
    child_attachment: ChildThreadAttachment,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    turn_start_changed: Arc<Notify>,
    process: Arc<ProcessGuard>,
    shutdown_started: AtomicBool,
    pricing_refresh: Option<crate::pricing::PricingRefresh>,
}

impl CodexSession {
    /// Interrupts every followed child thread's latest turn. The interrupts go
    /// out together and each is bounded by the interrupt's own timeout; one
    /// Codex refuses or never answers is passed over, because stopping the
    /// rest matters more than any single child — the same terms Claude stops
    /// its background tasks on. A child whose stream has named no turn yet is
    /// skipped, there being no turn a `turn/interrupt` could address.
    async fn interrupt_children(&self) {
        let targets = self
            .correlation
            .lock()
            .expect("Codex native correlation lock is not poisoned")
            .child_interrupt_targets();
        let interrupts = targets.into_iter().filter_map(|(thread_id, turn_id)| {
            let turn_id = turn_id?;
            self.transport.questionnaires().end_thread(&thread_id);
            let transport = self.transport.clone();
            let bound = self.context.timeouts.interrupt_request;
            Some(async move {
                let _ = transport
                    .request_with_timeout(
                        "turn/interrupt",
                        &TurnInterruptParams {
                            thread_id: &thread_id,
                            turn_id: &turn_id,
                        },
                        bound,
                    )
                    .await;
            })
        });
        futures_util::future::join_all(interrupts).await;
    }

    /// Sends `input` to child thread `thread_id` as a `turn/start` of its own,
    /// having attached the thread first where `attach` asks it, and answers
    /// the native turn Codex took the input into.
    async fn start_subagent_turn(
        &self,
        thread_id: &str,
        input: &[super::wire::UserInput],
        attach: bool,
    ) -> Result<String, ProviderError> {
        if attach && let Some(model) = self.child_attachment.request_stream(thread_id).await? {
            self.correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned")
                .note_settled_child_model(thread_id, model);
        }
        let posture = *self
            .context
            .posture
            .lock()
            .expect("Codex Session posture lock is not poisoned");
        let result = self
            .transport
            .request(
                "turn/start",
                &SubagentTurnStartParams {
                    thread_id,
                    input,
                    approval_policy: posture.approval_policy(),
                    sandbox_policy: posture.sandbox_policy(),
                },
            )
            .await
            .map_err(|error| {
                codex_error_context("Codex did not take the Subagent's input", error)
            })?;
        let started: TurnStartResult = serde_json::from_value(result)
            .map_err(|_| codex_error("Codex returned an invalid turn/start response"))?;
        if started.turn.id.is_empty() {
            return Err(codex_error(
                "Codex returned an invalid turn/start response: Turn ID was empty",
            ));
        }
        Ok(started.turn.id)
    }
}

impl ProviderSession for CodexSession {
    fn update_approval_posture(
        &self,
        posture: crate::protocol::ApprovalPosture,
        has_active_work: bool,
    ) -> ProviderFuture<'_, crate::provider::ProviderPostureApplication> {
        Box::pin(async move {
            let crate::protocol::ApprovalPosture::Codex {
                approval_policy,
                sandbox_mode,
            } = posture
            else {
                return Err(codex_error("Approval Posture belongs to another Provider"));
            };
            *self
                .context
                .posture
                .lock()
                .expect("Codex Session posture lock is not poisoned") = CodexPosture {
                approval_policy,
                sandbox_mode,
            };
            Ok(if has_active_work {
                crate::provider::ProviderPostureApplication::NextTurn
            } else {
                crate::provider::ProviderPostureApplication::Applied
            })
        })
    }

    fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> ProviderFuture<'_, ProviderDecisionDelivery> {
        Box::pin(async move {
            let native = self.transport.submit_decision(id, decision).await?;
            let transport = self.transport.clone();
            let interrupt_timeout = self.context.timeouts.interrupt_request;
            Ok(ProviderDecisionDelivery::with_follow_up(Box::pin(
                async move {
                    let super::transport::NativeDecisionDelivery {
                        settlement,
                        interrupt,
                    } = native;
                    // Release native completion before sending an interrupt
                    // whose response shares the same reader.
                    drop(settlement);
                    let Some(target) = interrupt else {
                        return Ok(());
                    };
                    transport
                        .request_with_timeout(
                            "turn/interrupt",
                            &TurnInterruptParams {
                                thread_id: &target.thread_id,
                                turn_id: &target.turn_id,
                            },
                            interrupt_timeout,
                        )
                        .await
                        .map_err(|error| {
                            codex_error_context("Codex permission interruption failed", error)
                        })?;
                    Ok(())
                },
            )))
        })
    }

    fn submit_questionnaire(
        &self,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.transport.submit_questionnaire(id, submission).await })
    }

    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if let Some(crate::protocol::ApprovalPosture::Codex {
                approval_policy,
                sandbox_mode,
            }) = input.approval_posture.as_ref()
            {
                *self
                    .context
                    .posture
                    .lock()
                    .expect("Codex Session posture lock is not poisoned") = CodexPosture {
                    approval_policy: *approval_policy,
                    sandbox_mode: *sandbox_mode,
                };
            }
            {
                let mut correlation = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned");
                if self.shutdown_started.load(Ordering::Acquire) {
                    return Err(codex_error("Codex Session is shutting down"));
                }
                correlation.begin_turn_start()?;
                correlation.context_fill_turn = Some(input.turn_id);
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
            let task = tokio::spawn(start_native_turn(NativeTurnStartRequest {
                thread_id: self.thread_id.clone(),
                input: input.input,
                selection: input.selection,
                summary,
                posture: *self
                    .context
                    .posture
                    .lock()
                    .expect("Codex Session posture lock is not poisoned"),
                skills: self.context.skills.clone(),
                execution_directory: self.context.execution_directory.clone(),
                transport: self.transport.clone(),
                correlation: self.correlation.clone(),
                turn_start_changed: self.turn_start_changed.clone(),
            }));
            task.await
                .map_err(|error| codex_error(format!("Codex Turn startup task failed: {error}")))?
        })
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let native_input = lower_input(
                &self.context.skills,
                &self.context.execution_directory,
                input.input,
            )
            .await?;
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
                        input: &native_input,
                        expected_turn_id: &turn_id,
                    },
                )
                .await
                .map_err(|error| codex_error_context("Codex Turn steering failed", error))?;
            let steered: TurnSteerResult = serde_json::from_value(result)
                .map_err(|_| codex_error("Codex returned an invalid turn/steer response"))?;
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
            self.transport.questionnaires().clear();
            self.transport.approvals().clear();
            // The interrupt ends the Session's own turn and leaves the child
            // threads it spawned running, so the children go first — the
            // established ordering every Provider keeps.
            self.interrupt_children().await;
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

    fn stop_subagents(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.interrupt_children().await;
            Ok(())
        })
    }

    /// Codex never has a Watch: its background terminals outlive a Turn, but nothing they do
    /// wakes the agent (ADR 0030), so no Codex Session is Monitoring and there is nothing to stop.
    fn stop_watches(
        &self,
        _watches: Vec<crate::provider::ProviderWatchId>,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn stop_subagent(&self, subagent_id: ProviderSubagentId) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let target = self
                .correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned")
                .child_interrupt_target(subagent_id.as_str());
            // A thread no longer followed has settled and has nothing left to
            // stop, and stopping nothing succeeds.
            let Some((thread_id, turn_id)) = target else {
                return Ok(());
            };
            // A followed child whose stream has named no turn yet leaves
            // nothing a `turn/interrupt` could address — but it is still
            // running. Succeeding here would settle its row while the child
            // works on, so the stop refuses instead: the record stays
            // truthful, and a retry once the child streams finds a turn.
            let Some(turn_id) = turn_id else {
                return Err(codex_error(
                    "Codex Subagent stop failed: the Subagent has not begun a turn to interrupt yet",
                ));
            };
            self.transport.questionnaires().end_thread(&thread_id);
            self.transport
                .request_with_timeout(
                    "turn/interrupt",
                    &TurnInterruptParams {
                        thread_id: &thread_id,
                        turn_id: &turn_id,
                    },
                    self.context.timeouts.interrupt_request,
                )
                .await
                .map_err(|error| codex_error_context("Codex Subagent stop failed", error))?;
            Ok(())
        })
    }

    /// Hands `input` to the native Subagent on child thread `subagent_id` as
    /// the thread's own input: a `turn/start` on that thread, which Codex
    /// takes on a collab (v1) child's thread and refuses a multi-agent v2
    /// sub-agent's. A settled child begins a native turn with it, which the
    /// projection routes into the stretch Suru wakes the child into, and a
    /// working one's running turn is steered with it; either way the input —
    /// a Subagent Report — arrives on the thread as nothing the projection
    /// shows (ADR 0035). It goes as `turn/start` even to a working child,
    /// rather than the `turn/steer` a working Turn of the Session's own
    /// takes: a Report a native Subagent's Provider refuses is dropped, not
    /// held, and a steer pinned to a child turn that has just ended would be
    /// refused where `turn/start` begins another. A child with no stretch
    /// open is attached again first, so the turn it begins streams here.
    fn deliver_to_subagent(
        &self,
        subagent_id: ProviderSubagentId,
        input: ProviderInput,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = subagent_id.as_str();
            let native_input = lower_input(
                &self.context.skills,
                &self.context.execution_directory,
                input,
            )
            .await?;
            let handed = user_input_text(&native_input);
            let attach = {
                let mut correlation = self
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned");
                if self.shutdown_started.load(Ordering::Acquire) {
                    return Err(codex_error("Codex Session is shutting down"));
                }
                correlation.expect_handed_input(thread_id, handed.clone())?
            };
            let taken = self
                .start_subagent_turn(thread_id, &native_input, attach)
                .await;
            let mut correlation = self
                .correlation
                .lock()
                .expect("Codex native correlation lock is not poisoned");
            match taken {
                Ok(turn_id) => {
                    correlation.handed_input_taken(thread_id, turn_id);
                    Ok(())
                }
                Err(error) => {
                    correlation.forget_handed_input(thread_id, &handed);
                    Err(error)
                }
            }
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if let Some(refresh) = &self.pricing_refresh {
                refresh.stop();
            }
            self.transport.questionnaires().clear();
            self.transport.approvals().clear();
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

struct NativeTurnStartRequest {
    thread_id: String,
    input: ProviderInput,
    selection: AgentSelection,
    summary: &'static str,
    posture: CodexPosture,
    skills: CodexSkills,
    execution_directory: std::path::PathBuf,
    transport: JsonRpcTransport,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    turn_start_changed: Arc<Notify>,
}

/// A Turn's input as the items Codex takes on `turn/start` or `turn/steer`: the
/// Subagent Reports it opens with as one text item at the head, then the
/// Prompt's text and each Skill it invokes.
async fn lower_input(
    skills: &CodexSkills,
    execution_directory: &std::path::Path,
    input: ProviderInput,
) -> Result<Vec<super::wire::UserInput>, ProviderError> {
    use super::wire::UserInput;
    input
        .lower(
            "Codex",
            |reports| vec![UserInput::Text { text: reports }],
            async |prompt, head| {
                let mut lowered = head
                    .map(|text| UserInput::Text { text })
                    .into_iter()
                    .collect::<Vec<_>>();
                lowered.extend(skills.lower(execution_directory, prompt)?);
                Ok(lowered)
            },
        )
        .await
}

async fn start_native_turn(request: NativeTurnStartRequest) -> Result<(), ProviderError> {
    let NativeTurnStartRequest {
        thread_id,
        input,
        selection,
        summary,
        posture,
        skills,
        execution_directory,
        transport,
        correlation,
        turn_start_changed,
    } = request;
    let started = async {
        let options = lower_turn_options(&selection)?;
        let native_input = lower_input(&skills, &execution_directory, input).await?;
        let result = transport
            .request(
                "turn/start",
                &TurnStartParams {
                    thread_id: &thread_id,
                    input: &native_input,
                    model: selection.model.as_str(),
                    approval_policy: posture.approval_policy(),
                    summary,
                    sandbox_policy: posture.sandbox_policy(),
                    effort: options.effort,
                    service_tier: options.service_tier,
                },
            )
            .await
            .map_err(|error| codex_error_context("Codex Turn startup failed", error))?;
        let started: TurnStartResult = serde_json::from_value(result)
            .map_err(|_| codex_error("Codex returned an invalid turn/start response"))?;
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
