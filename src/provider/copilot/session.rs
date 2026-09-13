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

use github_copilot_sdk::{
    DeliveryMode, MessageOptions, ResumeSessionConfig, SessionConfig,
    SessionId as CopilotSessionId, SetModelOptions, rpc::CurrentModel,
    session::Session as NativeSession, session_events::ContextTier,
};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, timeout};

use super::{
    CONTEXT_TIER_OPTION_ID, COPILOT_AGENT_ID, COPILOT_CLIENT_NAME, COPILOT_HARNESS_NAME,
    COPILOT_PROVIDER_ID, REASONING_EFFORT_OPTION_ID,
    catalog::{model_descriptors, tier_id},
    copilot_error, copilot_error_context,
    projection::{CopilotCorrelation, provider_events},
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
        ProviderError, ProviderFuture, ProviderResumeState, ProviderSession,
        ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
        harness::SharedHarnessHandle,
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

/// What a failure opening the Copilot Session is reported under, which is the operation Suru asked
/// for rather than the phase of it that went wrong.
const STARTUP_CONTEXT: &str = "Copilot Session startup failed";
const RESUME_CONTEXT: &str = "Copilot Session resume failed";

/// Opens a Copilot Session on the shared harness process `handle` was granted from — resuming the
/// one the Resume State names when the Suru Session was restored with one, and creating a fresh one
/// otherwise.
///
/// Both paths install Suru's permission bridge before the native Session opens, so requests that
/// arrive during creation and after resume follow the fixed posture captured for this Session.
pub(super) async fn start_copilot_session(
    handle: SharedHarnessHandle<CopilotConnection>,
    request: ProviderSessionRequest,
    skills: CopilotSkills,
    interrupt_request_timeout: Duration,
    permissions: CopilotPermissions,
) -> Result<ProviderSessionConnection, ProviderError> {
    let handle = Arc::new(handle);
    let (request_events, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let questionnaires = Arc::new(super::questionnaire::CopilotQuestionnaires::new(
        request_events.clone(),
    ));
    let execution_directory = request.execution_directory.clone();
    let approvals = Arc::new(super::approval::CopilotApprovals::new(
        request_events,
        permissions,
        execution_directory.clone(),
    ));
    let (context, copilot_session_id, native) = match known_session_id(request.resume_state)? {
        // A restored Suru Session keeps the identifier its Copilot Session was created under,
        // because that is what Copilot filed the work under. A resume that fails is not an
        // invitation to start over: a Suru Session that quietly opened an empty Copilot Session
        // would read as continuous while having forgotten everything, so the failure stands and the
        // Transcript the Session was restored with stays readable.
        Some(session_id) => {
            let config = ResumeSessionConfig::new(session_id.clone())
                .with_client_name(COPILOT_CLIENT_NAME)
                .with_working_directory(request.execution_directory)
                .with_streaming(true)
                // Pinned rather than left to the CLI's default: a Subagent's deltas are its
                // child Session's whole Transcript.
                .with_include_sub_agent_streaming_events(true)
                .with_enable_config_discovery(true)
                .with_enable_skills(true)
                .with_permission_handler(approvals.clone())
                .with_user_input_handler(questionnaires.clone());
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
            let config = SessionConfig::default()
                .with_session_id(session_id.clone())
                .with_client_name(COPILOT_CLIENT_NAME)
                .with_working_directory(request.execution_directory)
                .with_streaming(true)
                // Pinned rather than left to the CLI's default: a Subagent's deltas are its
                // child Session's whole Transcript.
                .with_include_sub_agent_streaming_events(true)
                .with_enable_config_discovery(true)
                .with_enable_skills(true)
                .with_permission_handler(approvals.clone())
                .with_user_input_handler(questionnaires.clone());
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

    let correlation = Arc::new(StdMutex::new(CopilotCorrelation::with_pricing(
        handle.connection().pricing(),
    )));
    let events = provider_events(
        subscription,
        handle.clone(),
        event_drain,
        correlation.clone(),
        skills.clone(),
        approvals.clone(),
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
    let session = Arc::new(CopilotSession {
        questionnaires,
        approvals,
        native,
        handle,
        correlation,
        selection: StdMutex::new(in_force),
        skills,
        execution_directory,
        interrupt_request_timeout,
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

struct CopilotSession {
    questionnaires: Arc<super::questionnaire::CopilotQuestionnaires>,
    approvals: Arc<super::approval::CopilotApprovals>,
    native: Arc<NativeSession>,
    handle: Arc<SharedHarnessHandle<CopilotConnection>>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    /// The Agent Selection in force on the Copilot Session — nothing until the CLI has resolved a
    /// Model — which the next Turn switches away from when it was selected under a different one.
    selection: StdMutex<Option<AgentSelection>>,
    skills: CopilotSkills,
    execution_directory: PathBuf,
    /// How long an interrupt waits for Copilot to acknowledge it before giving up.
    interrupt_request_timeout: Duration,
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
    fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: Decision,
    ) -> ProviderFuture<'_, crate::provider::ProviderDecisionDelivery> {
        Box::pin(async move {
            let delivery = self.approvals.submit(&self.native, id, decision).await?;
            let native = self.native.clone();
            let handle = self.handle.clone();
            let approvals = self.approvals.clone();
            let questionnaires = self.questionnaires.clone();
            let interrupt_timeout = self.interrupt_request_timeout;
            Ok(crate::provider::ProviderDecisionDelivery::with_follow_up(
                Box::pin(async move {
                    drop(delivery.settlement);
                    if decision != Decision::DeclineAndInterrupt {
                        return Ok(());
                    }
                    questionnaires.cancel();
                    CopilotSession::abort_native_loop(
                        &native,
                        &handle,
                        &approvals,
                        interrupt_timeout,
                        "Copilot Turn interruption failed",
                    )
                    .await
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
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .begin_turn()?;
            let started = async {
                self.apply_selection(&input.selection).await?;
                let prompt = self
                    .skills
                    .expand(
                        &self.handle,
                        &self.execution_directory,
                        &self.native,
                        crate::protocol::SkillPromptDelivery::Initial,
                        input.prompt,
                    )
                    .await?;
                self.correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .context_prompt_ready(input.turn_id);
                until_crash(
                    &self.handle,
                    "Copilot Turn startup failed",
                    self.native.send(prompt.as_str()),
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

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.require_running_turn("steer")?;
            let prompt = self
                .skills
                .expand(
                    &self.handle,
                    &self.execution_directory,
                    &self.native,
                    crate::protocol::SkillPromptDelivery::Steer,
                    input.prompt,
                )
                .await?;
            // Immediate delivery injects the Prompt into the loop already running, where Copilot's
            // default would hold it back and run it as a Turn of its own once this one stopped.
            //
            // Nothing here names the Turn being steered, because Copilot's send does not take one:
            // a steer that reaches the CLI after its loop has stopped falls back to that default
            // and begins a Turn Suru is no longer expecting. Codex pins its steer to the Turn it
            // meant and lets the Provider reject a stale one; the closest this wire comes is
            // refusing to send once Suru's own Turn has settled, which leaves the stretch between
            // Copilot stopping and Suru hearing about it.
            until_crash(
                &self.handle,
                "Copilot Turn steering failed",
                self.native
                    .send(MessageOptions::new(prompt).with_mode(DeliveryMode::Immediate)),
            )
            .await
            .map(|_message_id| ())
        })
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.require_running_turn("interrupt")?;
            self.questionnaires.cancel();
            // The Turn is Copilot's whole agentic loop, so stopping it is the whole-loop abort. The
            // Turn settles on the aborted idle that follows, not on this acknowledgement — which is
            // why an unanswered abort is bounded here rather than left to the loop to end.
            Self::abort_native_loop(
                &self.native,
                &self.handle,
                &self.approvals,
                self.interrupt_request_timeout,
                "Copilot Turn interruption failed",
            )
            .await
        })
    }

    fn stop_subagents(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            // Copilot addresses no single subagent, so stopping the Session's
            // late-running delegations is the same whole-loop abort an
            // interrupt is — with the Turn already settled, the loop holds
            // nothing else to lose. Deliberately not gated on a running Turn,
            // because this is exactly the stop that arrives after one.
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
