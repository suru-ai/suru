//! One Copilot Session on the shared harness process, and the Turns it runs.
//!
//! Copilot keeps the Agent Selection on the Session rather than taking it per request: the Model,
//! its reasoning effort, and its context tier are Session state the CLI holds between Turns. A
//! Turn whose Selection differs from the one in force therefore switches the Session onto it
//! before delivering the Prompt, which is also how a Session created before Suru knew the
//! Selection — [`ProviderSessionRequest`] carries none — arrives at the right Model. A fresh
//! Session may report no Model at all, because Suru creates it without one and the CLI resolves
//! nothing until asked: that is a Session with no Selection in force, not a failure, and the first
//! Turn puts one in force by switching.
//!
//! Copilot addresses its own Session by the identifier the client gave it at creation and keeps
//! the work behind it on disk, so that identifier is the whole of Suru's Resume State here: a Suru
//! Session that was restored asks Copilot to resume its Copilot Session rather than to create one,
//! and it picks up where it stopped.

use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use github_copilot_sdk::{
    Attachment, DeliveryMode, MessageOptions, ResumeSessionConfig, SessionConfig,
    SessionId as CopilotSessionId, SetModelOptions, ToolSearchConfig,
    rpc::{CurrentModel, HistoryCompactRequest, HistoryCompactRequestTrigger, TasksCancelRequest},
    session::Session as NativeSession,
    session_events::ContextTier,
};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, timeout};

use super::{
    CONTEXT_TIER_OPTION_ID, COPILOT_AGENT_ID, COPILOT_CLIENT_NAME, COPILOT_FAILURE_FALLBACK,
    COPILOT_HARNESS_NAME, COPILOT_PROVIDER_ID, REASONING_EFFORT_OPTION_ID,
    broker::{broker_mcp_servers, broker_system_message},
    catalog::{model_descriptors, tier_id},
    copilot_error, copilot_error_context,
    projection::{
        CopilotCorrelation, ManualCompactionAnswer, ManualCompactionAnswers, TaskRosterSource,
        provider_events,
    },
    skills::CopilotSkills,
    transport::CopilotConnection,
};
use crate::{
    protocol::{
        AgentId, AgentIdentity, AgentSelection, CopilotPermissions, Decision, ModelDescriptor,
        ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue,
        ProviderId,
    },
    provider::{
        AttributedProviderEvent, ProviderAttachment, ProviderCompactionInput, ProviderError,
        ProviderFuture, ProviderInterruption, ProviderResumeState, ProviderSession,
        ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
        ProviderWatchId, concise_remote_message, harness::SharedHarnessHandle, headed_text,
    },
};

/// How long an interrupt waits for the CLI to acknowledge the whole-loop abort. Long enough for a
/// busy loop to answer, short enough that a user who asked for the work to stop is not left
/// watching a Turn that is never going to settle.
pub(super) const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Everything Suru must remember about a Copilot Session to continue it after a restart: the
/// identifier Copilot files it under. Suru never reads what Copilot keeps behind that identifier,
/// which is why this is all the Resume State carries.
#[derive(Deserialize, Serialize)]
struct CopilotResumeState {
    session_id: CopilotSessionId,
}

/// Tool search turned on outright rather than left to the CLI's default. Left unset, whether a turn
/// defers MCP tools behind `tool_search_tool` falls to rollout flags the CLI resolves per Model, and
/// a Session that resolves them off loads every discovered MCP server's tool definitions up front:
/// tens of thousands of tokens where the CLI on its own spends a few hundred. Enabling it still
/// leaves a Model that cannot search tools to load them eagerly.
fn enabled_tool_search() -> ToolSearchConfig {
    ToolSearchConfig::new().with_enabled(true)
}

/// What a failure opening the Copilot Session is reported under, which is the operation Suru asked
/// for rather than the phase of it that went wrong.
const STARTUP_CONTEXT: &str = "Copilot Session startup failed";
const RESUME_CONTEXT: &str = "Copilot Session resume failed";

/// Opens a Copilot Session on the shared harness process `handle` was granted from — resuming the
/// one the Resume State names when the Suru Session was restored with one, and creating a fresh one
/// otherwise.
///
/// Both paths install Suru's permission bridge before the native Session opens, so requests that
/// arrive during creation and after resume follow the fixed posture captured for this Session. Both
/// hand the Session the Broker, when this start was handed it, in the server list Copilot takes on
/// create and resume alike: a resume carries the token this start was handed, and a CLI process
/// resuming the Session connects anew with the headers the resume names
/// (docs/validation/0408-copilot-mcp-tool-timeout.md).
pub(super) async fn start_copilot_session(
    handle: SharedHarnessHandle<CopilotConnection>,
    request: ProviderSessionRequest,
    skills: CopilotSkills,
    interrupt_request_timeout: Duration,
    permissions: CopilotPermissions,
) -> Result<ProviderSessionConnection, ProviderError> {
    let permissions = match request.approval_posture.as_ref() {
        Some(crate::protocol::ApprovalPosture::Copilot { permissions }) => *permissions,
        _ => permissions,
    };
    let handle = Arc::new(handle);
    let (request_events, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let local_events = request_events.clone();
    let questionnaires = Arc::new(super::questionnaire::CopilotQuestionnaires::new(
        request_events.clone(),
    ));
    let execution_directory = request.execution_directory.clone();
    let approvals = Arc::new(super::approval::CopilotApprovals::new(
        request_events,
        permissions,
        execution_directory.clone(),
    ));
    let broker = request.broker.as_ref().map(broker_mcp_servers);
    let broker_note = request.broker.as_ref().map(broker_system_message);
    let (context, copilot_session_id, native) = match known_session_id(request.resume_state)? {
        // A restored Suru Session keeps the identifier its Copilot Session was created under,
        // because that is what Copilot filed the work under. A resume that fails is not an
        // invitation to start over: a Suru Session that quietly opened an empty Copilot Session
        // would read as continuous while having forgotten everything, so the failure stands and the
        // Transcript the Session was restored with stays readable.
        Some(session_id) => {
            let mut config = ResumeSessionConfig::new(session_id.clone())
                .with_client_name(COPILOT_CLIENT_NAME)
                .with_working_directory(request.execution_directory)
                .with_streaming(true)
                // Pinned rather than left to the CLI's default: a Subagent's deltas are its
                // child Session's whole Transcript.
                .with_include_sub_agent_streaming_events(true)
                .with_enable_config_discovery(true)
                .with_enable_skills(true)
                .with_tool_search(enabled_tool_search())
                .with_permission_handler(approvals.clone())
                .with_user_input_handler(questionnaires.clone());
            config.mcp_servers = broker;
            config.system_message = broker_note;
            let native = until_crash(
                &handle,
                RESUME_CONTEXT,
                handle.connection().client().resume_session(config),
            )
            .await?;
            (RESUME_CONTEXT, session_id, native)
        }
        // The SDK registers the identifier before it asks the CLI to create the Session, which is
        // what gives the Session-scoped requests the CLI may issue mid-creation somewhere to land.
        None => {
            let session_id = CopilotSessionId::new(uuid::Uuid::new_v4().to_string());
            let mut config = SessionConfig::default()
                .with_session_id(session_id.clone())
                .with_client_name(COPILOT_CLIENT_NAME)
                .with_working_directory(request.execution_directory)
                .with_streaming(true)
                // Pinned rather than left to the CLI's default: a Subagent's deltas are its
                // child Session's whole Transcript.
                .with_include_sub_agent_streaming_events(true)
                .with_enable_config_discovery(true)
                .with_enable_skills(true)
                .with_tool_search(enabled_tool_search())
                .with_permission_handler(approvals.clone())
                .with_user_input_handler(questionnaires.clone());
            config.mcp_servers = broker;
            config.system_message = broker_note;
            let native = until_crash(
                &handle,
                STARTUP_CONTEXT,
                handle.connection().client().create_session(config),
            )
            .await?;
            (STARTUP_CONTEXT, session_id, native)
        }
    };
    let native = Arc::new(native);
    let current = until_crash(&handle, context, native.rpc().model().get_current()).await?;
    let in_force = agent_selection(current);
    let selection = match &in_force {
        Some(selection) => selection.clone(),
        None => default_selection(&handle, context).await?,
    };
    handle.connection().ensure_pricing().await;
    let subscription = native.subscribe();
    let event_drain = handle
        .connection()
        .event_checkpoint(copilot_session_id.as_str());
    let resume_state = ProviderResumeState::new(
        serde_json::to_value(CopilotResumeState {
            session_id: copilot_session_id,
        })
        .expect("Copilot Resume State serialization is infallible"),
    );

    let correlation = Arc::new(StdMutex::new(CopilotCorrelation::working_in(
        execution_directory.clone(),
        handle.connection().pricing(),
        selection.clone(),
    )));
    let (events, compaction_answers) = provider_events(
        subscription,
        handle.clone(),
        event_drain,
        correlation.clone(),
        skills.clone(),
        approvals.clone(),
        TaskRosterSource {
            native: native.clone(),
            request_timeout: interrupt_request_timeout,
        },
    );
    let question_lifecycle = questionnaires.clone();
    let approval_lifecycle = approvals.clone();
    let events = futures_util::StreamExt::inspect(events, move |event| {
        if let Ok(crate::provider::AttributedProviderEvent { attribution, event }) = event
            && matches!(
                event,
                crate::provider::ProviderEvent::TurnCompleted
                    | crate::provider::ProviderEvent::TurnInterrupted
                    | crate::provider::ProviderEvent::TurnFailed { .. }
            )
        {
            approval_lifecycle.settle(attribution);
        }
        if matches!(
            event,
            Ok(crate::provider::AttributedProviderEvent {
                attribution: crate::provider::ProviderEventAttribution::OwningSession,
                event: crate::provider::ProviderEvent::TurnCompleted
                    | crate::provider::ProviderEvent::TurnInterrupted
                    | crate::provider::ProviderEvent::TurnFailed { .. }
            })
        ) || event.is_err()
        {
            question_lifecycle.cancel();
            if event.is_err() {
                approval_lifecycle.clear();
            }
        }
    });
    let requests = futures_util::stream::unfold(request_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });
    let events = Box::pin(futures_util::stream::select(events, Box::pin(requests)));
    let interrupter = CopilotInterrupter {
        native: native.clone(),
        handle: handle.clone(),
        approvals: approvals.clone(),
        questionnaires: questionnaires.clone(),
        correlation: correlation.clone(),
        local_events: local_events.clone(),
        request_timeout: interrupt_request_timeout,
        in_flight: Arc::default(),
    };
    let session = Arc::new(CopilotSession {
        questionnaires,
        approvals,
        native,
        handle,
        correlation,
        interrupter,
        selection: StdMutex::new(in_force),
        skills,
        execution_directory,
        interrupt_request_timeout,
        local_events,
        compaction_answers,
        session_models_listed: tokio::sync::OnceCell::new(),
    });
    Ok(ProviderSessionConnection::new(
        AgentIdentity {
            agent: AgentId::new(COPILOT_AGENT_ID),
            selection,
        },
        Some(resume_state),
        session,
        events,
    ))
}

/// The Copilot Session `resume_state` names, or nothing when the Suru Session has never reached
/// Copilot. Resume State Suru cannot read is a failure rather than a reason to start over: the
/// Session it belongs to has work behind it that opening a new Copilot Session would abandon.
fn known_session_id(
    resume_state: Option<ProviderResumeState>,
) -> Result<Option<CopilotSessionId>, ProviderError> {
    let Some(state) = resume_state else {
        return Ok(None);
    };
    let state: CopilotResumeState = serde_json::from_value(state.into_payload())
        .map_err(|error| copilot_error(format!("Copilot Resume State is invalid: {error}")))?;
    if state.session_id.is_empty() {
        return Err(copilot_error(
            "Copilot Resume State is invalid: the Copilot Session identifier was empty",
        ));
    }
    Ok(Some(state.session_id))
}

/// Runs `work` against the CLI, giving up the moment the shared harness process hosting it dies:
/// once the process is gone nothing is left to answer, so its exit is the answer.
pub(super) async fn until_crash<T>(
    handle: &SharedHarnessHandle<CopilotConnection>,
    context: &str,
    work: impl Future<Output = Result<T, github_copilot_sdk::Error>>,
) -> Result<T, ProviderError> {
    tokio::select! {
        biased;
        crashed = handle.crashed() => Err(copilot_error_context(context, crashed)),
        done = work => done.map_err(|error| handle.connection().failure(context, error)),
    }
}

/// The Agent Selection the Copilot Session is running under, read back from the CLI so the
/// Selection Suru reports is the one Copilot actually resolved rather than the one Suru guessed —
/// or nothing, for a fresh Session the CLI has resolved no Model on yet.
fn agent_selection(current: CurrentModel) -> Option<AgentSelection> {
    let model = current.model_id.filter(|model| !model.is_empty())?;
    let mut options = Vec::new();
    if let Some(effort) = current.reasoning_effort {
        options.push(model_option_selection(REASONING_EFFORT_OPTION_ID, effort));
    }
    // A tier this SDK build cannot name has no choice ID to report it under.
    if let Some(tier) = current
        .context_tier
        .filter(|tier| *tier != ContextTier::Unknown)
    {
        options.push(model_option_selection(
            CONTEXT_TIER_OPTION_ID,
            tier_id(tier),
        ));
    }
    Some(AgentSelection {
        provider: ProviderId::new(COPILOT_PROVIDER_ID),
        model: ModelId::new(model),
        options,
    })
}

/// The Agent Selection a Session with no Model in force is reported under: the catalog's default,
/// which is what the first Turn will put in force when the user never chooses.
async fn default_selection(
    handle: &SharedHarnessHandle<CopilotConnection>,
    context: &str,
) -> Result<AgentSelection, ProviderError> {
    let listed = tokio::select! {
        biased;
        crashed = handle.crashed() => return Err(copilot_error_context(context, crashed)),
        listed = handle.connection().list_models() => {
            listed.map_err(|error| {
                let message = format!("{context}: {error}");
                error.reworded(message)
            })?
        }
    };
    model_descriptors(listed)
        .iter()
        .find(|descriptor| descriptor.is_default)
        .map(ModelDescriptor::default_agent_selection)
        .ok_or_else(|| {
            copilot_error(format!(
                "{context}: the Session reported no active Model and the catalog offers no default"
            ))
        })
}

fn model_option_selection(option: &str, choice: impl Into<String>) -> ModelOptionSelection {
    ModelOptionSelection {
        id: ModelOptionId::new(option),
        value: ModelOptionValue::Select {
            choice: ModelOptionChoiceId::new(choice.into()),
        },
    }
}

/// The context tier Copilot knows `choice` by, or nothing for a choice no tier this SDK build can
/// name — one Suru could not lower back over the wire.
fn context_tier(choice: &ModelOptionChoiceId) -> Option<ContextTier> {
    match serde_json::from_value(serde_json::Value::String(choice.as_str().to_owned())) {
        Ok(ContextTier::Unknown) | Err(_) => None,
        Ok(tier) => Some(tier),
    }
}

/// `message` carrying each of a Prompt's Attachments as a blob: its bytes in base64, its type,
/// and its label as the name Copilot shows it by. A Prompt without any sends no attachment list.
fn with_blobs(message: MessageOptions, attachments: Vec<ProviderAttachment>) -> MessageOptions {
    if attachments.is_empty() {
        return message;
    }
    message.with_attachments(
        attachments
            .into_iter()
            .map(|attachment| Attachment::Blob {
                data: STANDARD.encode(&attachment.bytes),
                mime_type: attachment.mime_type,
                display_name: Some(attachment.label),
            })
            .collect(),
    )
}

/// Stops the work one Copilot Session runs: its agentic loop, whose abort stops its Subagents
/// too, and the compaction Copilot runs in the background of the Session, which that abort leaves
/// running. Every interrupt goes through it — the one a declined Approval asks for included — so
/// none leaves a compaction running, or a Turn held open on one.
#[derive(Clone)]
struct CopilotInterrupter {
    native: Arc<NativeSession>,
    handle: Arc<SharedHarnessHandle<CopilotConnection>>,
    approvals: Arc<super::approval::CopilotApprovals>,
    questionnaires: Arc<super::questionnaire::CopilotQuestionnaires>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    local_events:
        tokio::sync::mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
    /// How long each request the interrupt makes waits for Copilot's answer.
    request_timeout: Duration,
    /// Held while an interrupt is at work.
    in_flight: Arc<tokio::sync::Mutex<()>>,
}

impl CopilotInterrupter {
    /// Stops whatever the Session runs, failing under `context` when Copilot does not.
    ///
    /// An interrupt can run while the Session goes on — the one a declined Approval asks for does
    /// — so the stretch it was for may settle on its own while Copilot answers the cancel. It is
    /// bound to that stretch, and the next Turn waits for it to resolve
    /// ([`Self::resolved`]), so nothing it still has to do reaches the Turn after.
    async fn interrupt(
        &self,
        context: &'static str,
    ) -> Result<ProviderInterruption, ProviderError> {
        let _in_flight = self.in_flight.lock().await;
        self.questionnaires.cancel();
        // A manual compaction runs no loop and is no background compaction, so it is stopped as
        // what it is, and nothing else is.
        if self
            .correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .is_compacting_on_request()
        {
            return self.abort_manual_compaction(context).await;
        }
        let scope = self
            .correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .begin_interrupt();
        // First, so the aborted idle has no compaction left to wait on. A cancel Copilot fails
        // stops nothing: the compaction is followed as before, and the loop left running.
        let mut cancelled = false;
        if scope.compacting {
            match self.cancel_compaction().await {
                Ok(answer) => cancelled = answer,
                Err(error) => {
                    let settled = self
                        .correlation
                        .lock()
                        .expect("Copilot correlation lock is not poisoned")
                        .cancel_failed();
                    self.settle_locally(settled);
                    return Err(error);
                }
            }
        }
        // A cancel that found nothing to cancel came too late: Copilot had already ended the
        // compaction, and its report of how, which may still be on its way, is the outcome. It is
        // waited for under the bound every request here waits under, so that it lands in the
        // Turn the compaction stood in, ahead of the idle of any abort sent below and of
        // anything of the Turn after; one that never comes leaves the compaction stopped.
        let end = self
            .correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .cancel_answered(scope, cancelled);
        if let Some(end) = end
            && timeout(self.request_timeout, end.notified()).await.is_err()
        {
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .compaction_end_missing();
        }
        let remainder = self
            .correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .finish_interrupt(scope, cancelled);
        // A loop that had already stopped, with only the compaction holding its Turn open, has
        // nothing left to report the Turn's end, so the Turn settles here — unless the
        // compaction had ended before the cancel reached it, and its end settles the Turn.
        self.settle_locally(remainder.settled);
        if !remainder.abort {
            self.approvals.clear();
            return Ok(if remainder.already_ended {
                ProviderInterruption::AlreadyEnded
            } else {
                ProviderInterruption::Stopped
            });
        }
        let idle = remainder.awaits_idle.then(|| {
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .expect_abort_idle()
        });
        // The Turn is Copilot's whole agentic loop, so stopping it is the whole-loop abort. The
        // Turn settles on the aborted idle that follows, not on this acknowledgement — which is
        // why an unanswered abort is bounded here rather than left to the loop to end.
        let aborted = CopilotSession::abort_native_loop(
            &self.native,
            &self.handle,
            &self.approvals,
            self.request_timeout,
            context,
        )
        .await;
        // An abort for Subagents working past a stopped loop settles that loop's Turn with the
        // idle it ends in, which the interrupt holds the next Turn back for: carried ahead of the
        // next Turn's work, it cannot land there. An abort that failed, or ended in no idle within
        // the bound every request here waits under, leaves the Turn to settle here instead.
        if let Some(idle) = idle {
            let carried =
                aborted.is_ok() && timeout(self.request_timeout, idle.notified()).await.is_ok();
            if !carried {
                let settled = self
                    .correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .abort_idle_missing(scope);
                self.settle_locally(settled);
            }
        }
        aborted.map(|()| ProviderInterruption::Stopped)
    }

    /// Waits for an interrupt still at work to resolve, holding off the next one while the caller
    /// keeps the guard: a Turn begun under it is safe from whatever the interrupt before it still
    /// had to stop.
    async fn resolved(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.in_flight.lock().await
    }

    fn settle_locally(&self, settled: Vec<AttributedProviderEvent>) {
        for event in settled {
            let _ = self.local_events.send(Ok(event));
        }
    }

    /// Aborts the manual compaction Copilot runs for `session.history.compact`
    /// (`session.history.abortManualCompaction`), bounded like an interrupt. Copilot then fails
    /// the compaction and the request as cancelled, which is the stop Suru asked for. An abort that
    /// finds nothing running stopped nothing: the compaction had already ended, a failure
    /// included, and its answer settles its Turn as it ended.
    async fn abort_manual_compaction(
        &self,
        context: &'static str,
    ) -> Result<ProviderInterruption, ProviderError> {
        let history = self.native.rpc().history();
        let abort = until_crash(&self.handle, context, history.abort_manual_compaction());
        let aborted = match timeout(self.request_timeout, abort).await {
            Ok(answered) => answered?.aborted,
            Err(_) => {
                return Err(copilot_error(format!(
                    "{context}: {COPILOT_HARNESS_NAME} timed out handling \
                     `session.history.abortManualCompaction`"
                )));
            }
        };
        self.correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .manual_compaction_aborted(aborted);
        Ok(if aborted {
            ProviderInterruption::Stopped
        } else {
            ProviderInterruption::AlreadyEnded
        })
    }

    /// Cancels the compaction Copilot is running in the background of the Session
    /// (`session.history.cancelBackgroundCompaction`), bounded like an interrupt, answering whether
    /// Copilot found it still running to cancel. Finding nothing means the compaction had already
    /// ended, its end on its way or already read.
    async fn cancel_compaction(&self) -> Result<bool, ProviderError> {
        const CONTEXT: &str = "Copilot compaction cancel failed";
        let history = self.native.rpc().history();
        let cancel = until_crash(
            &self.handle,
            CONTEXT,
            history.cancel_background_compaction(),
        );
        match timeout(self.request_timeout, cancel).await {
            Ok(answered) => answered.map(|answer| answer.cancelled),
            Err(_) => Err(copilot_error(format!(
                "{CONTEXT}: {COPILOT_HARNESS_NAME} timed out handling \
                 `session.history.cancelBackgroundCompaction`"
            ))),
        }
    }
}

struct CopilotSession {
    questionnaires: Arc<super::questionnaire::CopilotQuestionnaires>,
    approvals: Arc<super::approval::CopilotApprovals>,
    native: Arc<NativeSession>,
    handle: Arc<SharedHarnessHandle<CopilotConnection>>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    interrupter: CopilotInterrupter,
    /// The Agent Selection in force on the Copilot Session — nothing until the CLI has resolved a
    /// Model — which the next Turn switches away from when it was selected under a different one.
    selection: StdMutex<Option<AgentSelection>>,
    /// Whether the CLI has been asked for this Session's own Model catalog, which it judges a
    /// Model switch against: see [`CopilotSession::apply_selection`].
    session_models_listed: tokio::sync::OnceCell<()>,
    skills: CopilotSkills,
    execution_directory: PathBuf,
    /// How long an interrupt waits for Copilot to acknowledge it before giving up.
    interrupt_request_timeout: Duration,
    /// Where what this Session stopped settles, on the Session's own event stream, when Copilot's
    /// timeline will report nothing of it: a Watch whose shell it cancelled, and a Turn whose loop
    /// had stopped before the interrupt cancelled the compaction holding it open.
    local_events:
        tokio::sync::mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
    /// Where Copilot's answer to a manual compaction joins the Session's timeline.
    compaction_answers: ManualCompactionAnswers,
}

impl CopilotSession {
    async fn abort_native_loop(
        native: &NativeSession,
        handle: &SharedHarnessHandle<CopilotConnection>,
        approvals: &super::approval::CopilotApprovals,
        request_timeout: Duration,
        context: &'static str,
    ) -> Result<(), ProviderError> {
        let aborted = until_crash(handle, context, native.abort());
        let result = match timeout(request_timeout, aborted).await {
            Ok(aborted) => aborted,
            Err(_) => Err(copilot_error(format!(
                "{context}: {COPILOT_HARNESS_NAME} timed out handling `session.abort`"
            ))),
        };
        if result.is_ok() {
            approvals.clear();
        }
        result
    }

    /// Cancels one background shell through the Session's task roster, by the identity the roster
    /// listed it under, bounded like an interrupt. Copilot answering that it did not cancel the
    /// shell is a refusal: the shell runs on.
    async fn cancel_shell(&self, shell: &str) -> Result<(), ProviderError> {
        const CONTEXT: &str = "Copilot Watch stop failed";
        let tasks = self.native.rpc().tasks();
        let cancel = until_crash(
            &self.handle,
            CONTEXT,
            tasks.cancel(TasksCancelRequest {
                id: shell.to_owned(),
            }),
        );
        match timeout(self.interrupt_request_timeout, cancel).await {
            Ok(Ok(result)) if result.cancelled => Ok(()),
            Ok(Ok(_)) => Err(copilot_error(format!(
                "{CONTEXT}: Copilot declined to cancel background shell `{shell}`"
            ))),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(copilot_error(format!(
                "{CONTEXT}: {COPILOT_HARNESS_NAME} timed out handling `session.tasks.cancel`"
            ))),
        }
    }

    /// Fails unless a Turn is running for `operation` to act on. Copilot takes both live-Turn
    /// operations as Session-level requests, so nothing about them says which Turn they meant:
    /// delivered with no Turn running, a steer would begin one and an interrupt would stop
    /// whatever the Session picked up next.
    fn require_running_turn(&self, operation: &str) -> Result<(), ProviderError> {
        if self
            .correlation
            .lock()
            .expect("Copilot correlation lock is not poisoned")
            .is_turn_running()
        {
            return Ok(());
        }
        Err(copilot_error(format!(
            "Copilot has no active Turn to {operation}"
        )))
    }

    /// Puts `selection` in force on the Copilot Session, doing nothing when it already is.
    async fn apply_selection(&self, selection: &AgentSelection) -> Result<(), ProviderError> {
        if self
            .selection
            .lock()
            .expect("Copilot Agent Selection lock is not poisoned")
            .as_ref()
            == Some(selection)
        {
            return Ok(());
        }
        let options = lower_selection_options(selection)?;
        // The CLI judges a switch's reasoning effort against the catalog it has resolved for this
        // Session, and resolves that catalog only when asked for it: switched before then, it
        // refuses every effort as unsupported — `none` on a Model whose own listing offers it
        // included — where the same switch once the Session's Models have been listed goes
        // through and runs (CLI 1.0.88; docs/validation/copilot-cold-switch-effort.md). Its
        // own hosts list first, so list once here, before the first switch. Nothing in the answer
        // is needed: the catalog Suru offers from is the flat one, and a CLI that cannot list is
        // left to judge the switch as it will.
        self.session_models_listed
            .get_or_init(|| async {
                if let Err(error) = until_crash(
                    &self.handle,
                    "Copilot Session Model listing failed",
                    self.native.rpc().model().list(),
                )
                .await
                {
                    tracing::debug!(
                        %error,
                        "Copilot did not list this Session's Models before its Model switch"
                    );
                }
            })
            .await;
        until_crash(
            &self.handle,
            "Copilot Model selection failed",
            self.native
                .set_model(selection.model.as_str(), Some(options)),
        )
        .await?;
        *self
            .selection
            .lock()
            .expect("Copilot Agent Selection lock is not poisoned") = Some(selection.clone());
        Ok(())
    }
}

/// Asks Copilot to compact the Session's context now (`session.history.compact`, triggered as
/// manual, with the user's `instructions` for the summary as its `customInstructions` where there
/// are any) and reads its answer: whether it compacted, and if not, why — Copilot's own words where
/// it failed the request, which is how it reports a compaction an abort cancelled.
async fn compact_on_request(
    native: &NativeSession,
    handle: &SharedHarnessHandle<CopilotConnection>,
    instructions: Option<String>,
) -> ManualCompactionAnswer {
    const CONTEXT: &str = "Copilot compaction failed";
    let request = HistoryCompactRequest {
        trigger: Some(HistoryCompactRequestTrigger::Manual),
        custom_instructions: instructions,
        ..HistoryCompactRequest::default()
    };
    let rpc = native.rpc();
    let history = rpc.history();
    let answered = tokio::select! {
        biased;
        crashed = handle.crashed() => Err((copilot_error_context(CONTEXT, crashed).to_string(), false)),
        answered = history.compact_with_params(request) => {
            answered.map_err(|error| match error.message() {
                Some(message) if error.rpc_code().is_some() => (
                    concise_remote_message(message, COPILOT_FAILURE_FALLBACK),
                    error.rpc_code().is_some_and(rejects_the_request),
                ),
                _ => (handle.connection().failure(CONTEXT, error).to_string(), false),
            })
        }
    };
    match answered {
        Ok(compacted) if compacted.success => ManualCompactionAnswer::Compacted {
            summary: compacted.summary_content,
        },
        Ok(_) => ManualCompactionAnswer::NotCompacted {
            error: None,
            rejected: false,
        },
        Err((error, rejected)) => ManualCompactionAnswer::NotCompacted {
            error: Some(error),
            rejected,
        },
    }
}

/// Whether a JSON-RPC error code says the CLI rejected a request as one it could not take — no
/// such method, a malformed request, or parameters it could not read — before running any of it.
/// Any other failure may come from work the request began.
fn rejects_the_request(code: i32) -> bool {
    matches!(code, -32602..=-32600)
}

/// The Model Options an Agent Selection carries, in the shape Copilot takes them — which is the
/// same shape whether they are put in force by switching a running Session onto them or by opening
/// a Session under them, as an Errand does.
pub(super) fn lower_selection_options(
    selection: &AgentSelection,
) -> Result<SetModelOptions, ProviderError> {
    let mut lowered = SetModelOptions::default();
    for option in &selection.options {
        let ModelOptionValue::Select { choice } = &option.value else {
            return Err(ProviderError::selection_rejected(format!(
                "Copilot does not support toggle Model Option `{}`",
                option.id
            )));
        };
        match option.id.as_str() {
            REASONING_EFFORT_OPTION_ID if lowered.reasoning_effort.is_none() => {
                lowered.reasoning_effort = Some(choice.as_str().to_owned());
            }
            CONTEXT_TIER_OPTION_ID if lowered.context_tier.is_none() => {
                lowered.context_tier = Some(context_tier(choice).ok_or_else(|| {
                    ProviderError::selection_rejected(format!(
                        "Copilot does not offer context tier `{choice}`"
                    ))
                })?);
            }
            REASONING_EFFORT_OPTION_ID | CONTEXT_TIER_OPTION_ID => {
                return Err(ProviderError::selection_rejected(format!(
                    "Copilot Model Option `{}` was selected more than once",
                    option.id
                )));
            }
            _ => {
                return Err(ProviderError::selection_rejected(format!(
                    "Copilot does not support Model Option `{}`",
                    option.id
                )));
            }
        }
    }
    Ok(lowered)
}

impl ProviderSession for CopilotSession {
    fn update_approval_posture(
        &self,
        posture: crate::protocol::ApprovalPosture,
        _has_active_work: bool,
    ) -> ProviderFuture<'_, crate::provider::ProviderPostureApplication> {
        Box::pin(async move {
            let crate::protocol::ApprovalPosture::Copilot { permissions } = posture else {
                return Err(copilot_error(
                    "Approval Posture belongs to another Provider",
                ));
            };
            self.approvals.adopt_posture(permissions);
            Ok(crate::provider::ProviderPostureApplication::Applied)
        })
    }

    fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: Decision,
    ) -> ProviderFuture<'_, crate::provider::ProviderDecisionDelivery> {
        Box::pin(async move {
            let delivery = self.approvals.submit(&self.native, id, decision).await?;
            let interrupter = self.interrupter.clone();
            Ok(crate::provider::ProviderDecisionDelivery::with_follow_up(
                Box::pin(async move {
                    drop(delivery.settlement);
                    if decision != Decision::DeclineAndInterrupt {
                        return Ok(());
                    }
                    interrupter
                        .interrupt("Copilot Turn interruption failed")
                        .await
                        .map(|_| ())
                }),
            ))
        })
    }
    fn submit_questionnaire(
        &self,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.require_running_turn("answer")?;
            self.questionnaires.submit(id, submission)
        })
    }

    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if let Some(crate::protocol::ApprovalPosture::Copilot { permissions }) =
                input.approval_posture.as_ref()
            {
                self.approvals.adopt_posture(*permissions);
            }
            // The interrupt still at work on the Turn before must stop nothing of this one.
            let resolved = self.interrupter.resolved().await;
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .begin_turn()?;
            drop(resolved);
            let started = async {
                self.apply_selection(&input.selection).await?;
                // A Turn the Subagent Reports alone begin — the Continuation
                // a Report wakes — is sent in immediate mode, so a loop
                // Copilot is running on its own takes the Reports at once
                // rather than after it stops (ADR 0035). A Prompt keeps the
                // default delivery it always had, the Reports at its head.
                let reports_alone = input.input.prompt.is_none();
                let message = input
                    .input
                    .lower("Copilot", MessageOptions::new, async |mut prompt, head| {
                        let attachments = std::mem::take(&mut prompt.attachments);
                        let text = headed_text(
                            head,
                            self.skills
                                .expand(
                                    &self.handle,
                                    &self.execution_directory,
                                    &self.native,
                                    crate::protocol::SkillPromptDelivery::Initial,
                                    prompt,
                                )
                                .await?,
                        );
                        Ok(with_blobs(MessageOptions::new(text), attachments))
                    })
                    .await?;
                let message = if reports_alone {
                    message.with_mode(DeliveryMode::Immediate)
                } else {
                    message
                };
                self.correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .context_prompt_ready(input.turn_id, input.selection.clone());
                until_crash(
                    &self.handle,
                    "Copilot Turn startup failed",
                    self.native.send(message),
                )
                .await
                .map(|_message_id| ())
            }
            .await;
            if started.is_err() {
                self.correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .abandon_turn();
            }
            started
        })
    }

    fn context_breakdown(&self) -> ProviderFuture<'_, crate::protocol::ContextBreakdown> {
        Box::pin(async move {
            const CONTEXT: &str = "Copilot Context Breakdown failed";
            let rpc = self.native.rpc();
            let metadata = rpc.metadata();
            let read = until_crash(&self.handle, CONTEXT, metadata.get_context_attribution());
            let result = timeout(self.interrupt_request_timeout, read)
                .await
                .map_err(|_| {
                    copilot_error(format!(
                        "{CONTEXT}: {COPILOT_HARNESS_NAME} timed out handling \
                         `session.metadata.getContextAttribution`"
                    ))
                })??;
            let attribution = result.context_attribution.ok_or_else(|| {
                copilot_error(format!(
                    "{CONTEXT}: Copilot has not measured this Session's context yet"
                ))
            })?;
            let window = self
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .served_window();
            super::context::context_breakdown(&attribution, window)
        })
    }

    /// Copilot compacts a Session on request with `session.history.compact`, triggered as manual
    /// and handed the user's instructions as its `customInstructions` where there are any, which
    /// runs no stretch of the loop and reports no turn: Suru opens the Turn the compaction
    /// runs in, and Copilot's answer to the request settles it once the compaction's own reports
    /// are in. The request is answered only once the compaction ends, so it is left running while
    /// the Session goes on, and its answer joins the Session's timeline. Copilot would take a
    /// compaction mid-Turn, report success and lose it; Suru asks only while the Session is idle,
    /// and while Copilot owes no report of an earlier one.
    fn compact(&self, input: ProviderCompactionInput) -> ProviderFuture<'_, AgentSelection> {
        Box::pin(async move {
            // The interrupt still at work on the Turn before must stop nothing of this one.
            let resolved = self.interrupter.resolved().await;
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .begin_manual_compaction(input.turn_id)?;
            drop(resolved);
            // The compaction runs under the Turn's Agent Selection, as a Prompt's would.
            if let Err(error) = self.apply_selection(&input.selection).await {
                self.correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .abandon_turn();
                return Err(error);
            }
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .context_prompt_ready(input.turn_id, input.selection.clone());
            let native = self.native.clone();
            let handle = self.handle.clone();
            let answers = self.compaction_answers.clone();
            let bound = self.interrupt_request_timeout;
            let turn_id = input.turn_id;
            let instructions = input.instructions;
            tokio::spawn(async move {
                let answer = compact_on_request(&native, &handle, instructions).await;
                answers.answer(turn_id, answer, bound).await;
            });
            Ok(input.selection)
        })
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.require_running_turn("steer")?;
            let message = input
                .input
                .lower("Copilot", MessageOptions::new, async |mut prompt, head| {
                    let attachments = std::mem::take(&mut prompt.attachments);
                    let text = headed_text(
                        head,
                        self.skills
                            .expand(
                                &self.handle,
                                &self.execution_directory,
                                &self.native,
                                crate::protocol::SkillPromptDelivery::Steer,
                                prompt,
                            )
                            .await?,
                    );
                    Ok(with_blobs(MessageOptions::new(text), attachments))
                })
                .await?;
            // Immediate delivery injects the Prompt — its images among it — into the loop already
            // running, where Copilot's default would hold it back and run it as a Turn of its own
            // once this one stopped.
            //
            // Nothing here names the Turn being steered, because Copilot's send does not take one:
            // a steer that reaches the CLI after its loop has stopped falls back to that default
            // and begins a Turn Suru is no longer expecting. Codex pins its steer to the Turn it
            // meant and lets the Provider reject a stale one; the closest this wire comes is
            // refusing to send once Suru's own Turn has settled, which leaves the stretch between
            // Copilot stopping and Suru hearing about it.
            //
            // A Turn Copilot's compaction holds open past its loop's idle has not settled: the
            // steer runs the loop again, and the Turn is the loop's to settle from here.
            let idled = self
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .steer_delivering();
            let sent = until_crash(
                &self.handle,
                "Copilot Turn steering failed",
                self.native.send(message.with_mode(DeliveryMode::Immediate)),
            )
            .await
            .map(|_message_id| ());
            if sent.is_err() {
                self.correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .steer_undelivered(idled);
            }
            sent
        })
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ProviderInterruption> {
        Box::pin(async move {
            self.require_running_turn("interrupt")?;
            self.interrupter
                .interrupt("Copilot Turn interruption failed")
                .await
        })
    }

    fn stop_subagents(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            // Copilot addresses no single subagent, so stopping the Session's
            // late-running delegations is the same whole-loop abort an
            // interrupt is — with the Turn already settled, the loop holds
            // nothing else to lose. Deliberately not gated on a running Turn,
            // because this is exactly the stop that arrives after one. No
            // compaction runs in a Continuation this stop reaches: Copilot
            // owns any it compacts in, which an interrupt stops instead.
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .sending_abort();
            Self::abort_native_loop(
                &self.native,
                &self.handle,
                &self.approvals,
                self.interrupt_request_timeout,
                "Copilot Subagent stop failed",
            )
            .await
        })
    }

    /// Stops the background shells the Watches name through the Session's task roster
    /// (`session.tasks.cancel`, by the shell identity the roster listed it under) — the stop
    /// Copilot's own task view offers a background shell, attached or detached — without touching
    /// the loop, which runs nothing while the Session is only Monitoring. Every shell Copilot
    /// confirms cancelling settles as stopped on the event stream. One Copilot declines to cancel
    /// — it cannot signal a detached shell whose process identity it never learned — is left
    /// running and its Watch live, and the stop reports the refusal.
    fn stop_watches(&self, watches: Vec<ProviderWatchId>) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let shells = self
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .live_watches(&watches);
            let cancelled =
                futures_util::future::join_all(shells.iter().map(|shell| self.cancel_shell(shell)))
                    .await;
            let mut stopped = Vec::new();
            let mut failure = None;
            for (shell, cancelled) in shells.into_iter().zip(cancelled) {
                match cancelled {
                    Ok(()) => stopped.push(shell),
                    Err(error) => failure = Some(error),
                }
            }
            let settled = self
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .project_watches_stopped(&stopped);
            for event in settled {
                let _ = self.local_events.send(Ok(event));
            }
            failure.map_or(Ok(()), Err)
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            // The harness process is the runtime's and hosts every other Copilot Session, so a
            // Session that is done with it lets go of its own event loop and leaves the process
            // running. Copilot keeps its own Session on disk, which is what a later resume of the
            // Suru Session picks back up.
            self.questionnaires.cancel();
            self.native.stop_event_loop().await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CopilotSessionId, known_session_id};
    use crate::provider::ProviderResumeState;

    #[test]
    fn a_session_that_never_reached_copilot_opens_a_new_copilot_session() {
        assert_eq!(
            known_session_id(None).expect("no Resume State is not a failure"),
            None
        );
    }

    #[test]
    fn the_copilot_session_identifier_survives_a_round_trip_through_resume_state() {
        let state = ProviderResumeState::new(json!({ "session_id": "copilot-session" }));
        assert_eq!(
            known_session_id(Some(state)).expect("the Resume State is readable"),
            Some(CopilotSessionId::new("copilot-session"))
        );
    }

    #[test]
    fn resume_state_suru_cannot_read_fails_rather_than_opening_a_new_copilot_session() {
        for unusable in [
            json!({}),
            json!({ "session_id": "" }),
            json!("copilot-session"),
        ] {
            let error = known_session_id(Some(ProviderResumeState::new(unusable.clone())))
                .expect_err("unusable Resume State fails the Session startup");
            assert!(
                error
                    .to_string()
                    .contains("Copilot Resume State is invalid"),
                "{unusable} reports what is wrong with it: {error}"
            );
        }
    }
}
