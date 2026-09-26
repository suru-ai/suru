//! One Claude Session on its own supervised CLI process, and the Turns it runs.
//!
//! The Model and its reasoning effort are spawn-time flags on the CLI, so the Session launches no
//! process until its first Turn starts: the Turn's Agent Selection is what the child is spawned
//! under, the way Copilot puts a Selection in force at Turn start. A Turn selected under different
//! flags therefore runs on a child of its own — the running one is torn down and its replacement
//! resumes the same conversation, so the change takes effect while the conversation continues.
//!
//! Suru mints the provider-session UUID and passes it at spawn, and that identifier is the whole
//! of the Resume State: the CLI keeps the conversation on disk behind it, so a restored Session
//! asks the CLI to resume it rather than to mint another. A resume the CLI cannot honor is fatal,
//! as is Resume State Suru cannot read — a Session that quietly opened an empty conversation would
//! read as continuous while having forgotten everything.
//!
//! The child is launched under the Session's effective native permission mode, which later changes
//! reach the same process over its control channel. Suru's stdio permission-prompt channel and the
//! Session's Workspace are carried with it. A Prompt is
//! delivered as a stream-json user message; the Turn's output streams back through
//! [`super::projection`] and the CLI's terminal result message Settles it.
//!
//! Steering and interrupting act on the Turn the child is running, which nothing on this wire
//! names: a steer is simply another user message on the running loop's stdin, and the interrupt is
//! a Session-level control request. Both therefore refuse a Session with no Turn running rather
//! than acting on whatever came next, and both read what is running from [`super::turn_in_flight`].
//!
//! The background work an interrupt stops first is the running child's own: the CLI keeps its task
//! roster per process, runs that work in the process group Suru stops as a whole, and announces
//! nothing it did not start itself. So the Session forgets the roster whenever it stops a child —
//! for a Selection change as much as at shutdown — and an interrupt afterwards asks the new child
//! to stop only the work it has reported. The child's output ends behind everything it wrote, and
//! that end settles every Watch it was running as lost (ADR 0030), so a Session left Monitoring
//! by the old child's background work stops Monitoring once that work is gone.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time::Duration};

use super::{
    CLAUDE_AGENT_ID,
    availability::ClaudeAvailability,
    claude_error, claude_error_context,
    projection::{ClaudeProjection, provider_events},
    runtime::usable_claude_models,
    skills::ClaudeSkills,
    transport::{ClaudeConnection, ClaudeSettingSources, ConversationSink, StreamJsonTransport},
    turn_in_flight::TurnInFlight,
    wire::{ControlRequest, UserMessageEnvelope},
};
use crate::{
    protocol::{AgentId, AgentIdentity, AgentSelection, ClaudePermissionMode, ModelDescriptor},
    provider::{
        ProviderDecisionDelivery, ProviderError, ProviderFuture, ProviderResumeState,
        ProviderSession, ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput,
        ProviderSubagentId, ProviderTurnInput,
        harness::{ProcessGuard, ProcessRegistry},
    },
};

/// Everything Suru must remember about a Claude Session to continue it after a restart: the
/// provider-session UUID Suru minted and spawned the CLI under, and the conversation every agent
/// the Session spawned rides under. The CLI keeps the conversation on disk behind the UUID, which
/// is all it needs; the agents are what the projection needs, since a resumed agent's start names
/// only its task and the SendMessage that resumed it, never the spawn its conversation rides under.
///
/// Suru reports this the moment the Session connects, while the CLI writes the conversation only
/// once a Turn has run under the identifier. A Session whose every Turn failed before the CLI
/// recorded anything therefore carries Resume State for a conversation that does not exist, and
/// a later restart fails it the way any unhonorable resume is failed. That is the conservative
/// end of the rule rather than an oversight: Suru cannot tell a conversation that was never
/// written from one the CLI has lost, and quietly minting a new one is what this Provider must
/// never do. The agents are reported as each one spawns, through a Resume State revision.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ClaudeResumeState {
    pub(super) session_id: String,
    /// Every agent task the conversation has run as a Subagent, by task id — the Subagent's
    /// identity — against the `parent_tool_use_id` its conversation rides under: the id of the
    /// tool use that spawned it, which a resume does not change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) agents: BTreeMap<String, String>,
}

impl ClaudeResumeState {
    pub(super) fn to_provider(&self) -> ProviderResumeState {
        ProviderResumeState::new(
            serde_json::to_value(self).expect("Claude Resume State serialization is infallible"),
        )
    }
}

/// How a child addresses the provider session: minting the conversation under the identifier Suru
/// generated, or continuing the one the CLI already keeps under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderSessionSpawn {
    Mint,
    Resume,
}

impl ProviderSessionSpawn {
    /// The CLI flag that names the provider session for this kind of spawn. Resuming is asked for
    /// without `--fork-session`, which is what keeps the conversation under the identifier Suru
    /// minted and so keeps the Resume State good for every later child.
    fn identity_flag(self) -> &'static str {
        match self {
            Self::Mint => "--session-id",
            Self::Resume => "--resume",
        }
    }
}

/// The timeouts a [`super::ClaudeRuntime`] hands each Session it starts.
#[derive(Clone, Copy)]
pub(super) struct ClaudeTimings {
    /// How long a control request waits for the CLI to answer it.
    pub(super) control_request: Duration,
    pub(super) context_request: Duration,
    /// How long an interrupt — and each of the task stops that go ahead of it — waits.
    pub(super) interrupt_request: Duration,
}

pub(super) async fn start_claude_session(
    executable: OsString,
    request: ProviderSessionRequest,
    processes: ProcessRegistry,
    availability: ClaudeAvailability,
    skills: ClaudeSkills,
    timings: ClaudeTimings,
    permission_mode: ClaudePermissionMode,
) -> Result<ProviderSessionConnection, ProviderError> {
    let permission_mode = match request.approval_posture.as_ref() {
        Some(crate::protocol::ApprovalPosture::Claude { permission_mode }) => *permission_mode,
        _ => permission_mode,
    };
    // Read before anything is launched: Resume State Suru cannot read fails the startup outright
    // rather than after a discovery the Session will never use.
    let restored = restored_state(request.resume_state)?;
    // The Selection the Session reports before the user chooses one: the catalog default, which is
    // what the first Turn will spawn under when nothing else was chosen.
    let selection = default_selection(
        executable.clone(),
        processes.clone(),
        availability,
        timings.control_request,
    )
    .await?;
    // A restored Session keeps the identifier its conversation is filed under, because that is
    // what the CLI kept the work behind, and the agents it spawned, whose resumes the projection
    // must still route.
    let (resume, next_spawn) = match restored {
        Some(restored) => (restored, ProviderSessionSpawn::Resume),
        None => (
            ClaudeResumeState {
                session_id: uuid::Uuid::new_v4().to_string(),
                agents: BTreeMap::new(),
            },
            ProviderSessionSpawn::Mint,
        ),
    };
    let provider_session_id = resume.session_id.clone();
    let resume_state = resume.to_provider();

    let (conversation, messages) = tokio::sync::mpsc::unbounded_channel();
    let turn = TurnInFlight::new();
    let questionnaires = Arc::new(super::questionnaire::ClaudeQuestionnaires::default());
    let approvals = Arc::new(super::approval::ClaudeApprovals::default());
    let (context, reports) = super::context::ContextQueries::new(timings.context_request);
    let events = provider_events(
        messages,
        ClaudeProjection::new(turn.clone(), resume),
        questionnaires.clone(),
        approvals.clone(),
        request.execution_directory.clone(),
        context.clone(),
        reports,
    );
    let session = Arc::new(ClaudeSession {
        context,
        questionnaires,
        approvals,
        executable,
        processes,
        execution_directory: request.execution_directory,
        provider_session_id,
        conversation,
        child: Mutex::new(ChildSlot {
            running: None,
            next_spawn,
        }),
        turn,
        skills,
        permission_mode: StdMutex::new(permission_mode),
        posture_request_timeout: timings.control_request,
        interrupt_request_timeout: timings.interrupt_request,
        shutdown_started: AtomicBool::new(false),
    });
    Ok(ProviderSessionConnection::new(
        AgentIdentity {
            agent: AgentId::new(CLAUDE_AGENT_ID),
            selection,
        },
        Some(resume_state),
        session,
        events,
    ))
}

/// What `resume_state` restores — the provider session it names and the agents spawned in it — or
/// nothing when the Suru Session has never reached Claude. Resume State Suru cannot read is a
/// failure rather than a reason to start over: the Session it belongs to has a conversation behind
/// it that minting a fresh identifier would abandon while looking as though nothing was lost.
fn restored_state(
    resume_state: Option<ProviderResumeState>,
) -> Result<Option<ClaudeResumeState>, ProviderError> {
    let Some(state) = resume_state else {
        return Ok(None);
    };
    let state: ClaudeResumeState = serde_json::from_value(state.into_payload())
        .map_err(|error| claude_error(format!("Claude Resume State is invalid: {error}")))?;
    if state.session_id.is_empty() {
        return Err(claude_error(
            "Claude Resume State is invalid: the provider session identifier was empty",
        ));
    }
    Ok(Some(state))
}

/// The Agent Selection a Session with none chosen runs under: the catalog's default row with its
/// default Model Options, asked of the CLI the same way the catalog is — the availability probe
/// ahead of it included, so a Turn reaching a Claude the user has to install, sign in to, or update
/// fails with that condition rather than with whatever the CLI does about it.
async fn default_selection(
    executable: OsString,
    processes: ProcessRegistry,
    availability: ClaudeAvailability,
    control_request_timeout: Duration,
) -> Result<AgentSelection, ProviderError> {
    const CONTEXT: &str = "Claude Session startup failed";
    let models = usable_claude_models(executable, processes, availability, control_request_timeout)
        .await
        .map_err(|error| claude_error_context(CONTEXT, error))?
        .models;
    models
        .iter()
        .find(|descriptor| descriptor.is_default)
        .map(ModelDescriptor::default_agent_selection)
        .ok_or_else(|| claude_error(format!("{CONTEXT}: the Model catalog offers no default")))
}

struct ClaudeSession {
    context: Arc<super::context::ContextQueries>,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    approvals: Arc<super::approval::ClaudeApprovals>,
    executable: OsString,
    processes: ProcessRegistry,
    execution_directory: PathBuf,
    /// The provider-session UUID Suru minted, which the child is spawned under.
    provider_session_id: String,
    /// Where every child this Session spawns delivers its conversation, so the Session's event
    /// stream outlives any one process.
    conversation: ConversationSink,
    /// The child the Session's Turns run on, and how the next one addresses the conversation.
    child: Mutex<ChildSlot>,
    /// What the Session and the projection of its conversation agree on about the Turn in flight.
    turn: Arc<TurnInFlight>,
    skills: ClaudeSkills,
    permission_mode: StdMutex<ClaudePermissionMode>,
    posture_request_timeout: Duration,
    interrupt_request_timeout: Duration,
    shutdown_started: AtomicBool,
}

/// The CLI process a Session's Turns run on — none until its first Turn, and none again between a
/// Selection change's teardown and the spawn that replaces it — beside how the next spawn addresses
/// the conversation.
struct ChildSlot {
    running: Option<ClaudeChild>,
    /// Whether the CLI already keeps a conversation under this Session's identifier: a restored
    /// Session's does from the start, and every Session's does once a child has been spawned under
    /// it, so a replacement child resumes rather than asking the CLI to mint what it already has.
    next_spawn: ProviderSessionSpawn,
}

/// One spawned CLI process and the Agent Selection its spawn flags put in force.
struct ClaudeChild {
    transport: StreamJsonTransport,
    process: Arc<ProcessGuard>,
    selection: AgentSelection,
    permission_mode: ClaudePermissionMode,
}

impl ClaudeChild {
    /// Stops the process this child runs on, leaving the conversation it ran on the CLI's disk for
    /// the next child to resume. An intended stop like this one publishes no failure of its own,
    /// so the Session's event stream carries on into the child that replaces it.
    ///
    /// A stopped process's output is waited out to its end, which the transport delivers behind
    /// everything the process wrote: the end is where the projection settles the process's Watches
    /// as lost, so no child spawned afterwards can have its own work mistaken for the old one's.
    async fn stop(&self) -> Result<(), ProviderError> {
        self.process.begin_shutdown();
        self.transport.close().await;
        self.process.wait_until_stopped().await?;
        self.transport.drained().await;
        Ok(())
    }
}

impl ClaudeSession {
    /// Stops `child`, and with it every background task it was running: they share its process
    /// group, and none of them will ever report settling. The roster forgets them however the stop
    /// went, because the child is no longer the one this Session's interrupts reach.
    async fn stop_child(&self, child: &ClaudeChild) -> Result<(), ProviderError> {
        let stopped = child.stop().await;
        self.turn.tasks_died_with_process();
        stopped
    }

    async fn apply_permission_mode(
        &self,
        slot: &mut ChildSlot,
        permission_mode: ClaudePermissionMode,
        context: &'static str,
    ) -> Result<(), ProviderError> {
        if let Some(child) = slot.running.as_mut()
            && child.permission_mode != permission_mode
        {
            child
                .transport
                .control_request(
                    &ControlRequest::SetPermissionMode {
                        mode: permission_mode,
                    },
                    self.posture_request_timeout,
                )
                .await
                .map_err(|error| claude_error_context(context, error.into_error()))?;
            child.permission_mode = permission_mode;
        }
        *self
            .permission_mode
            .lock()
            .expect("Claude posture lock is not poisoned") = permission_mode;
        Ok(())
    }

    /// Stops every background task the CLI has reported running, so nothing the Turn spawned
    /// outlives the interrupt that follows it. The stops go out together and each is bounded by
    /// the interrupt's own timeout; one the CLI refuses or never answers is passed over, because
    /// stopping the loop matters more than any single task and a task Suru could not stop is one
    /// the CLI still has on its roster.
    async fn stop_background_tasks(&self, transport: &StreamJsonTransport) {
        let tasks = self.turn.live_tasks();
        let requests = tasks
            .iter()
            .map(|task_id| ControlRequest::StopTask {
                task_id: task_id.clone(),
            })
            .collect::<Vec<_>>();
        let stopped = futures_util::future::join_all(
            requests
                .iter()
                .map(|request| transport.control_request(request, self.interrupt_request_timeout)),
        )
        .await;
        for (task_id, stopped) in tasks.iter().zip(stopped) {
            // A task the CLI acknowledges stopping is off the roster now: its own notification
            // can lose the race with the interrupt that follows.
            if stopped.is_ok() {
                self.turn.task_settled(task_id);
            }
        }
    }
}

impl ProviderSession for ClaudeSession {
    fn update_approval_posture(
        &self,
        posture: crate::protocol::ApprovalPosture,
        _has_active_work: bool,
    ) -> ProviderFuture<'_, crate::provider::ProviderPostureApplication> {
        Box::pin(async move {
            let crate::protocol::ApprovalPosture::Claude { permission_mode } = posture else {
                return Err(claude_error("Approval Posture belongs to another Provider"));
            };
            let mut slot = self.child.lock().await;
            self.apply_permission_mode(
                &mut slot,
                permission_mode,
                "Claude Approval Posture update failed",
            )
            .await?;
            Ok(crate::provider::ProviderPostureApplication::Applied)
        })
    }

    fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> ProviderFuture<'_, ProviderDecisionDelivery> {
        Box::pin(async move { self.approvals.submit(id, decision).await })
    }

    fn submit_questionnaire(
        &self,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.questionnaires.submit(id, submission).await })
    }

    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Turn startup failed";
            self.context
                .begin_turn(input.turn_id, input.selection.model.as_str());
            let prompt = self.skills.lower(&self.execution_directory, input.prompt)?;
            let mut slot = self.child.lock().await;
            let permission_mode = match input.approval_posture.as_ref() {
                Some(crate::protocol::ApprovalPosture::Claude { permission_mode }) => {
                    *permission_mode
                }
                _ => *self
                    .permission_mode
                    .lock()
                    .expect("Claude posture lock is not poisoned"),
            };
            if self.shutdown_started.load(Ordering::Acquire) {
                return Err(claude_error("Claude Session is shutting down"));
            }
            // The Model and its Options are spawn-time flags, so only a Selection change replaces
            // the child. Permission mode is a live control; restarting for it would kill background
            // work and turn a retryable control failure into a conversation restart.
            if slot
                .running
                .as_ref()
                .is_none_or(|child| child.selection != input.selection)
            {
                // The Selection is lowered onto flags before anything is torn down, so one the CLI
                // has no flags for leaves the Session running on the child it had.
                let args = spawn_args(
                    &self.provider_session_id,
                    &input.selection,
                    slot.next_spawn,
                    permission_mode,
                )?;
                if let Some(previous) = slot.running.take() {
                    self.stop_child(&previous)
                        .await
                        .map_err(|error| claude_error_context(CONTEXT, error))?;
                }
                // A child that carries a resume is failing at the resume when it cannot be
                // launched, which is what the Turn should say went wrong.
                let launch_context = match slot.next_spawn {
                    ProviderSessionSpawn::Mint => CONTEXT,
                    ProviderSessionSpawn::Resume => "Claude Session resume failed",
                };
                let ClaudeConnection { transport, process } = StreamJsonTransport::launch(
                    &self.executable,
                    args,
                    Some(self.execution_directory.clone()),
                    Some(self.conversation.clone()),
                    self.processes.clone(),
                    ClaudeSettingSources::PersonalAndProject,
                )
                .await
                .map_err(|error| claude_error_context(launch_context, error))?;
                // The conversation is the CLI's to keep from here on, so every later child resumes
                // it rather than asking for it to be minted again.
                self.context.connect(transport.clone());
                self.questionnaires.connect(transport.clone());
                self.approvals.connect(transport.clone());
                slot.next_spawn = ProviderSessionSpawn::Resume;
                slot.running = Some(ClaudeChild {
                    transport,
                    process,
                    selection: input.selection.clone(),
                    permission_mode,
                });
            }
            self.apply_permission_mode(
                &mut slot,
                permission_mode,
                "Claude Turn permission mode update failed",
            )
            .await?;
            let child = slot
                .running
                .as_ref()
                .expect("a Turn runs on the child that was just spawned for it");
            self.turn.begin_turn(input.selection);
            self.context.ready();
            child
                .transport
                .send(&UserMessageEnvelope::text(&prompt))
                .await
                .map_err(|error| {
                    // The Prompt never reached the CLI, so the Turn it would have begun is not
                    // running and is owed nothing.
                    self.context.abandon();
                    self.turn.abandon_turn();
                    claude_error_context(CONTEXT, error)
                })
        })
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Turn steering failed";
            if !input.prompt.skill_invocations.is_empty() {
                return Err(claude_error(
                    "Claude does not support Skill Invocations for Steer Prompts; queue this Prompt instead",
                ));
            }
            // A steer is another user message on the running loop's stdin, and nothing about one
            // says which Turn it joins: delivered to a Session running no Turn, the CLI would
            // answer it as a Turn of its own that Suru never began. A Turn only ever runs on a
            // spawned child, so a Session without one is running none either.
            let slot = self.child.lock().await;
            let Some(child) = slot.running.as_ref() else {
                return Err(no_live_turn("steer"));
            };
            if !self.turn.accept_steer() {
                return Err(no_live_turn("steer"));
            }
            child
                .transport
                .send(&UserMessageEnvelope::text(&input.prompt.text))
                .await
                .map_err(|error| {
                    self.turn.withdraw_prompt();
                    claude_error_context(CONTEXT, error)
                })
        })
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Turn interruption failed";
            let transport = {
                let slot = self.child.lock().await;
                slot.running
                    .as_ref()
                    .filter(|_| self.turn.is_running())
                    .map(|child| child.transport.clone())
            };
            let Some(transport) = transport else {
                return Err(no_live_turn("interrupt"));
            };
            // The interrupt ends the loop and leaves everything it spawned running, so the
            // background work goes first. The Turn settles on the terminal result that follows,
            // not on this acknowledgement — which is why an unanswered interrupt is bounded here
            // rather than left to the loop to end.
            self.questionnaires.clear();
            self.approvals.clear();
            self.stop_background_tasks(&transport).await;
            transport
                .control_request(
                    &ControlRequest::Interrupt {
                        cancel_queued: true,
                    },
                    self.interrupt_request_timeout,
                )
                .await
                .map(|_receipt| ())
                .map_err(|failure| {
                    // An interrupt Suru could not deliver fails the Turn, so the Turn is over
                    // whatever the loop does next: nothing the CLI still owes it is its to settle,
                    // and a later steer must not join a Turn Suru has already closed.
                    self.turn.abandon_turn();
                    claude_error_context(CONTEXT, failure.into_error())
                })
        })
    }

    fn stop_subagents(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            // Deliberately not gated on a running Turn: this is the interrupt
            // that arrives after the Turn settled, when the roster holds
            // exactly the work that outlived it. No child process means
            // nothing is running, and nothing to stop is success.
            let transport = {
                let slot = self.child.lock().await;
                slot.running.as_ref().map(|child| child.transport.clone())
            };
            if let Some(transport) = transport {
                self.questionnaires.clear();
                self.approvals.clear();
                self.stop_background_tasks(&transport).await;
            }
            Ok(())
        })
    }

    fn stop_subagent(&self, subagent_id: ProviderSubagentId) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Subagent stop failed";
            let transport = {
                let slot = self.child.lock().await;
                slot.running.as_ref().map(|child| child.transport.clone())
            };
            // A Subagent off the roster — settled, or running on a process no
            // longer there — has nothing left to stop, and stopping nothing
            // succeeds; the caller settles the row either way.
            let Some(transport) = transport else {
                return Ok(());
            };
            let Some(task_id) = self.turn.subagent_task(subagent_id.as_str()) else {
                return Ok(());
            };
            self.questionnaires
                .settle(&crate::provider::ProviderEventAttribution::Subagent(
                    subagent_id.clone(),
                ));
            self.approvals
                .settle(&crate::provider::ProviderEventAttribution::Subagent(
                    subagent_id.clone(),
                ));
            transport
                .control_request(
                    &ControlRequest::StopTask {
                        task_id: task_id.clone(),
                    },
                    self.interrupt_request_timeout,
                )
                .await
                .map_err(|failure| claude_error_context(CONTEXT, failure.into_error()))?;
            // Acknowledged means off the CLI's roster: its own notification
            // can lose the race with what the caller does next.
            self.turn.task_settled(&task_id);
            Ok(())
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.shutdown_started.store(true, Ordering::Release);
            self.context.disconnect();
            self.questionnaires.clear();
            self.approvals.clear();
            let slot = self.child.lock().await;
            let Some(child) = slot.running.as_ref() else {
                return Ok(());
            };
            self.stop_child(child).await
        })
    }
}

/// The failure a live-Turn operation gives when the Session is running no Turn for it to act on.
/// Both operations are Session-level requests that name no Turn, so a Session with none refuses
/// them rather than acting on whatever the CLI picks up next.
fn no_live_turn(operation: &str) -> ProviderError {
    claude_error(format!("Claude has no active Turn to {operation}"))
}

/// The flags one Turn's Agent Selection spawns the child under, beside the conversation `spawn`
/// addresses and the fixed permission posture every Claude Session launches with.
fn spawn_args(
    provider_session_id: &str,
    selection: &AgentSelection,
    spawn: ProviderSessionSpawn,
    permission_mode: ClaudePermissionMode,
) -> Result<Vec<OsString>, ProviderError> {
    let mut args: Vec<OsString> = [
        "--include-partial-messages",
        "--permission-mode",
        permission_mode.as_wire_value(),
        "--permission-prompt-tool",
        "stdio",
        spawn.identity_flag(),
        provider_session_id,
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.extend(super::selection_args(selection)?);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use serde_json::json;

    use super::{ClaudeResumeState, ProviderSessionSpawn, restored_state, spawn_args};
    use crate::{
        protocol::{
            AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
            ModelOptionValue, ProviderId,
        },
        provider::ProviderResumeState,
    };

    #[cfg(unix)]
    mod live_operations {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        use tokio::{sync::Mutex, time::Duration};

        use super::{
            super::{ChildSlot, ClaudeSession, ClaudeSkills, ProviderSessionSpawn, TurnInFlight},
            selection,
        };
        use crate::provider::{
            ProviderSession, ProviderSteerInput, ProviderTurnInput, harness::ProcessRegistry,
        };

        /// A stand-in CLI that consumes its stdin and exits when it closes, which is all a
        /// shutdown needs from the child.
        fn scripted_session(directory: &tempfile::TempDir) -> Arc<ClaudeSession> {
            use std::os::unix::fs::PermissionsExt;
            let executable = directory.path().join("claude");
            std::fs::write(
                &executable,
                "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
            )
            .expect("write scripted Claude");
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
                .expect("make scripted Claude executable");
            let mut processes = ProcessRegistry::new("Claude Code CLI");
            processes.set_exit_grace(Duration::from_millis(200));
            let (conversation, _messages) = tokio::sync::mpsc::unbounded_channel();
            Arc::new(ClaudeSession {
                context: crate::provider::claude::context::ContextQueries::new(
                    Duration::from_millis(200),
                )
                .0,
                questionnaires: Arc::new(
                    crate::provider::claude::questionnaire::ClaudeQuestionnaires::default(),
                ),
                approvals: Arc::new(crate::provider::claude::approval::ClaudeApprovals::default()),
                executable: executable.into(),
                processes,
                execution_directory: directory.path().to_owned(),
                provider_session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
                conversation,
                child: Mutex::new(ChildSlot {
                    running: None,
                    next_spawn: ProviderSessionSpawn::Mint,
                }),
                turn: TurnInFlight::new(),
                skills: ClaudeSkills::default(),
                permission_mode: std::sync::Mutex::new(
                    crate::protocol::ClaudePermissionMode::Default,
                ),
                posture_request_timeout: Duration::from_millis(200),
                interrupt_request_timeout: Duration::from_millis(200),
                shutdown_started: AtomicBool::new(false),
            })
        }

        #[tokio::test]
        async fn steering_a_session_running_no_turn_fails_rather_than_beginning_one() {
            let directory = tempfile::tempdir().expect("create scripted Claude directory");
            let session = scripted_session(&directory);
            let error = session
                .steer_turn(ProviderSteerInput {
                    prompt: crate::provider::ProviderPrompt::plain("Answer in French instead"),
                })
                .await
                .expect_err("a Session with no Turn running has none to steer");
            assert!(
                error.to_string().contains("no active Turn to steer"),
                "{error}"
            );

            session
                .start_turn(ProviderTurnInput {
                    turn_id: crate::protocol::TurnId::new(),
                    prompt: crate::provider::ProviderPrompt::plain("Say hello"),
                    selection: selection(Vec::new()),
                    approval_posture: None,
                })
                .await
                .expect("the first Turn spawns the child");
            session
                .steer_turn(ProviderSteerInput {
                    prompt: crate::provider::ProviderPrompt::plain("Answer in French instead"),
                })
                .await
                .expect("the running Turn takes the steer");

            session.shutdown().await.expect("shutdown succeeds");
        }

        #[tokio::test]
        async fn interrupting_a_session_running_no_turn_fails_rather_than_stopping_the_next() {
            let directory = tempfile::tempdir().expect("create scripted Claude directory");
            let session = scripted_session(&directory);
            let error = session
                .interrupt_turn()
                .await
                .expect_err("a Session with no Turn running has none to interrupt");
            assert!(
                error.to_string().contains("no active Turn to interrupt"),
                "{error}"
            );
        }

        #[tokio::test]
        async fn shutdown_before_any_child_spawns_is_idempotent() {
            let directory = tempfile::tempdir().expect("create scripted Claude directory");
            let session = scripted_session(&directory);
            session.shutdown().await.expect("first shutdown succeeds");
            session
                .shutdown()
                .await
                .expect("repeated shutdown succeeds");
        }

        #[tokio::test]
        async fn shutdown_terminates_a_spawned_child_and_repeats_cleanly() {
            let directory = tempfile::tempdir().expect("create scripted Claude directory");
            let session = scripted_session(&directory);
            session
                .start_turn(ProviderTurnInput {
                    turn_id: crate::protocol::TurnId::new(),
                    prompt: crate::provider::ProviderPrompt::plain("Say hello"),
                    selection: selection(Vec::new()),
                    approval_posture: None,
                })
                .await
                .expect("the first Turn spawns the child");
            session.shutdown().await.expect("first shutdown succeeds");
            session
                .shutdown()
                .await
                .expect("repeated shutdown finds the child already down and stays clean");
            assert!(session.shutdown_started.load(Ordering::Acquire));
            let error = session
                .start_turn(ProviderTurnInput {
                    turn_id: crate::protocol::TurnId::new(),
                    prompt: crate::provider::ProviderPrompt::plain("Too late"),
                    selection: selection(Vec::new()),
                    approval_posture: None,
                })
                .await
                .expect_err("a shut-down Session refuses further Turns");
            assert!(error.to_string().contains("shutting down"), "{error}");
        }
    }

    fn selection(options: Vec<ModelOptionSelection>) -> AgentSelection {
        AgentSelection {
            provider: ProviderId::new("claude"),
            model: ModelId::new("fixture[1m]"),
            options,
        }
    }

    fn effort(level: &str) -> ModelOptionSelection {
        ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(level),
            },
        }
    }

    #[test]
    fn a_selection_lowers_onto_the_spawn_flags_the_cli_takes() {
        let args = spawn_args(
            "11111111-2222-3333-4444-555555555555",
            &selection(vec![effort("low")]),
            ProviderSessionSpawn::Mint,
            crate::protocol::ClaudePermissionMode::Default,
        )
        .expect("a reasoning-effort selection lowers");
        assert_eq!(
            args,
            [
                "--include-partial-messages",
                "--permission-mode",
                "default",
                "--permission-prompt-tool",
                "stdio",
                "--session-id",
                "11111111-2222-3333-4444-555555555555",
                "--model",
                "fixture[1m]",
                "--effort",
                "low",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn a_child_on_a_conversation_the_cli_already_holds_spawns_resuming_it() {
        let args = spawn_args(
            "11111111-2222-3333-4444-555555555555",
            &selection(Vec::new()),
            ProviderSessionSpawn::Resume,
            crate::protocol::ClaudePermissionMode::Default,
        )
        .expect("a resuming spawn lowers");
        assert_eq!(
            args[5..7],
            ["--resume", "11111111-2222-3333-4444-555555555555"].map(OsString::from),
            "the child continues the conversation rather than asking for a new one: {args:?}"
        );
        assert!(
            !args.contains(&OsString::from("--fork-session")),
            "resuming keeps the conversation under the identifier the Resume State names"
        );
    }

    #[test]
    fn a_selection_without_effort_spawns_no_effort_flag() {
        let args = spawn_args(
            "id",
            &selection(Vec::new()),
            ProviderSessionSpawn::Mint,
            crate::protocol::ClaudePermissionMode::Default,
        )
        .expect("an effortless selection lowers");
        assert!(!args.contains(&OsString::from("--effort")));
    }

    #[test]
    fn a_model_option_claude_does_not_know_is_rejected() {
        let error = spawn_args(
            "id",
            &selection(vec![ModelOptionSelection {
                id: ModelOptionId::new("context_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("long"),
                },
            }]),
            ProviderSessionSpawn::Mint,
            crate::protocol::ClaudePermissionMode::Default,
        )
        .expect_err("an unknown Model Option is a rejected selection");
        assert!(error.is_selection_rejected());
        assert!(error.to_string().contains("context_tier"), "{error}");
    }

    #[test]
    fn a_model_option_selected_twice_is_rejected() {
        let error = spawn_args(
            "id",
            &selection(vec![effort("low"), effort("high")]),
            ProviderSessionSpawn::Mint,
            crate::protocol::ClaudePermissionMode::Default,
        )
        .expect_err("a duplicated Model Option is a rejected selection");
        assert!(error.is_selection_rejected());
    }

    #[test]
    fn a_session_that_never_reached_claude_mints_a_conversation_of_its_own() {
        assert_eq!(
            restored_state(None).expect("no Resume State is not a failure"),
            None
        );
    }

    #[test]
    fn the_provider_session_identifier_survives_a_round_trip_through_resume_state() {
        let state = ProviderResumeState::new(json!({
            "session_id": "11111111-2222-3333-4444-555555555555",
        }));
        assert_eq!(
            restored_state(Some(state)).expect("the Resume State is readable"),
            Some(ClaudeResumeState {
                session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
                agents: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn the_agents_a_session_spawned_survive_a_round_trip_through_resume_state() {
        let state = ClaudeResumeState {
            session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
            agents: BTreeMap::from([("a2046dbbe8ecd4a5c".to_owned(), "toolu_agent".to_owned())]),
        };
        assert_eq!(
            restored_state(Some(state.to_provider())).expect("the Resume State is readable"),
            Some(state)
        );
    }

    #[test]
    fn resume_state_suru_cannot_read_fails_rather_than_opening_a_new_conversation() {
        for unusable in [
            json!({}),
            json!({ "session_id": "" }),
            json!({ "session_id": 7 }),
            json!("11111111-2222-3333-4444-555555555555"),
        ] {
            let error = restored_state(Some(ProviderResumeState::new(unusable.clone())))
                .expect_err("unusable Resume State fails the Session startup");
            assert!(
                error.to_string().contains("Claude Resume State is invalid"),
                "{unusable} reports what is wrong with it: {error}"
            );
        }
    }
}
