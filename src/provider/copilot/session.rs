//! One Copilot Session on the shared harness process, and the Turns it runs.
//!
//! Copilot keeps the Agent Selection on the Session rather than taking it per request: the Model,
//! its reasoning effort, and its context tier are Session state the CLI holds between Turns. A
//! Turn whose Selection differs from the one in force therefore switches the Session onto it
//! before delivering the Prompt, which is also how a Session created before Suru knew the
//! Selection — [`ProviderSessionRequest`] carries none — arrives at the right Model.

use std::{
    future::Future,
    sync::{Arc, Mutex as StdMutex},
};

use github_copilot_sdk::{
    DeliveryMode, MessageOptions, SessionConfig, SessionId as CopilotSessionId, SetModelOptions,
    rpc::CurrentModel, session::Session as NativeSession, session_events::ContextTier,
};
use tokio::time::{Duration, timeout};

use super::{
    CONTEXT_TIER_OPTION_ID, COPILOT_AGENT_ID, COPILOT_CLIENT_NAME, COPILOT_HARNESS_NAME,
    COPILOT_PROVIDER_ID, REASONING_EFFORT_OPTION_ID,
    catalog::tier_id,
    copilot_error, copilot_error_context,
    projection::{CopilotCorrelation, provider_events},
    transport::CopilotConnection,
};
use crate::{
    protocol::{
        AgentId, AgentIdentity, AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionSelection, ModelOptionValue, ProviderId,
    },
    provider::{
        ProviderError, ProviderFuture, ProviderSession, ProviderSessionConnection,
        ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
        harness::SharedHarnessHandle,
    },
};

/// How long an interrupt waits for the CLI to acknowledge the whole-loop abort. Long enough for a
/// busy loop to answer, short enough that a user who asked for the work to stop is not left
/// watching a Turn that is never going to settle.
pub(super) const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens a Copilot Session on the shared harness process `handle` was granted from.
pub(super) async fn start_copilot_session(
    handle: SharedHarnessHandle<CopilotConnection>,
    request: ProviderSessionRequest,
    interrupt_request_timeout: Duration,
) -> Result<ProviderSessionConnection, ProviderError> {
    let handle = Arc::new(handle);
    // Copilot resumes a Session by its own identifier, so #121 stores this one as Resume State;
    // until then every Session starts a fresh Copilot conversation.
    let _ = request.resume_state;
    // The SDK registers the identifier before it asks the CLI to create the Session, which is what
    // gives the Session-scoped requests the CLI may issue mid-creation somewhere to land.
    let copilot_session_id = CopilotSessionId::new(uuid::Uuid::new_v4().to_string());
    let config = SessionConfig::default()
        .with_session_id(copilot_session_id)
        .with_client_name(COPILOT_CLIENT_NAME)
        .with_working_directory(request.workspace)
        .with_streaming(true)
        // Full auto, matching the Codex posture: the harness answers Copilot's permission requests
        // itself and no approval concept crosses the Provider seam.
        .approve_all_permissions();

    let native = until_crash(
        &handle,
        "Copilot Session startup failed",
        handle.connection().client().create_session(config),
    )
    .await?;
    let current = until_crash(
        &handle,
        "Copilot Session startup failed",
        native.rpc().model().get_current(),
    )
    .await?;
    let selection = agent_selection(current)?;

    let correlation = Arc::new(StdMutex::new(CopilotCorrelation::new()));
    let events = provider_events(native.subscribe(), handle.clone(), correlation.clone());
    let session = Arc::new(CopilotSession {
        native,
        handle,
        correlation,
        selection: StdMutex::new(selection.clone()),
        interrupt_request_timeout,
    });
    Ok(ProviderSessionConnection::new(
        AgentIdentity {
            agent: AgentId::new(COPILOT_AGENT_ID),
            selection,
        },
        None,
        session,
        events,
    ))
}

/// Runs `work` against the CLI, giving up the moment the shared harness process hosting it dies:
/// once the process is gone nothing is left to answer, so its exit is the answer.
async fn until_crash<T>(
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
/// Selection Suru reports is the one Copilot actually resolved rather than the one Suru guessed.
fn agent_selection(current: CurrentModel) -> Result<AgentSelection, ProviderError> {
    let Some(model) = current.model_id.filter(|model| !model.is_empty()) else {
        return Err(copilot_error(
            "Copilot Session startup failed: the Session reported no active Model",
        ));
    };
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
    Ok(AgentSelection {
        provider: ProviderId::new(COPILOT_PROVIDER_ID),
        model: ModelId::new(model),
        options,
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
    native: NativeSession,
    handle: Arc<SharedHarnessHandle<CopilotConnection>>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    /// The Agent Selection in force on the Copilot Session, which the next Turn switches away from
    /// when it was selected under a different one.
    selection: StdMutex<AgentSelection>,
    /// How long an interrupt waits for Copilot to acknowledge it before giving up.
    interrupt_request_timeout: Duration,
}

impl CopilotSession {
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
        if *self
            .selection
            .lock()
            .expect("Copilot Agent Selection lock is not poisoned")
            == *selection
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
            .expect("Copilot Agent Selection lock is not poisoned") = selection.clone();
        Ok(())
    }
}

/// The Model Options an Agent Selection carries, in the shape Copilot's Model switch takes them.
fn lower_selection_options(selection: &AgentSelection) -> Result<SetModelOptions, ProviderError> {
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
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .begin_turn()?;
            let started = async {
                self.apply_selection(&input.selection).await?;
                until_crash(
                    &self.handle,
                    "Copilot Turn startup failed",
                    self.native.send(input.prompt.as_str()),
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
                    .send(MessageOptions::new(input.prompt).with_mode(DeliveryMode::Immediate)),
            )
            .await
            .map(|_message_id| ())
        })
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.require_running_turn("interrupt")?;
            // The Turn is Copilot's whole agentic loop, so stopping it is the whole-loop abort. The
            // Turn settles on the aborted idle that follows, not on this acknowledgement — which is
            // why an unanswered abort is bounded here rather than left to the loop to end.
            const CONTEXT: &str = "Copilot Turn interruption failed";
            let aborted = until_crash(&self.handle, CONTEXT, self.native.abort());
            match timeout(self.interrupt_request_timeout, aborted).await {
                Ok(aborted) => aborted,
                Err(_elapsed) => Err(copilot_error(format!(
                    "{CONTEXT}: {COPILOT_HARNESS_NAME} timed out handling `session.abort`"
                ))),
            }
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            // The harness process is the runtime's and hosts every other Copilot Session, so a
            // Session that is done with it lets go of its own event loop and leaves the process
            // running. Copilot keeps the conversation on disk, which is what #121 resumes from.
            self.native.stop_event_loop().await;
            Ok(())
        })
    }
}
