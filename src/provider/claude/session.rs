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
//! The child is launched full-auto — permissions bypassed, matching the house posture — with the
//! Suru Session's Workspace as its working directory. A Prompt is delivered as a stream-json user
//! message; the Turn's output streams back through [`super::projection`] and the CLI's terminal
//! result message Settles it.
//!
//! Steering and interrupting act on the Turn the child is running, which nothing on this wire
//! names: a steer is simply another user message on the running loop's stdin, and the interrupt is
//! a Session-level control request. Both therefore refuse a Session with no Turn running rather
//! than acting on whatever came next, and both read what is running from [`super::turn_in_flight`].

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time::Duration};

use super::{
    CLAUDE_AGENT_ID, CLAUDE_PROVIDER_ID, REASONING_EFFORT_OPTION_ID, claude_error,
    claude_error_context,
    projection::provider_events,
    runtime::discover_claude_models,
    transport::{ClaudeConnection, ConversationSink, StreamJsonTransport},
    turn_in_flight::TurnInFlight,
    wire::{ControlRequest, UserMessageEnvelope},
};
use crate::{
    protocol::{AgentId, AgentIdentity, AgentSelection, ModelDescriptor, ModelOptionValue},
    provider::{
        ProviderError, ProviderFuture, ProviderResumeState, ProviderSession,
        ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
        harness::{ProcessGuard, ProcessRegistry},
    },
};

/// Everything Suru must remember about a Claude Session to continue it after a restart: the
/// provider-session UUID Suru minted and spawned the CLI under. The CLI keeps the conversation on
/// disk behind it, which is why this is all the Resume State carries.
///
/// Suru reports this the moment the Session connects, while the CLI writes the conversation only
/// once a Turn has run under the identifier. A Session whose every Turn failed before the CLI
/// recorded anything therefore carries Resume State for a conversation that does not exist, and
/// a later restart fails it the way any unhonorable resume is failed. That is the conservative
/// end of the rule rather than an oversight: Suru cannot tell a conversation that was never
/// written from one the CLI has lost, and quietly minting a new one is what this Provider must
/// never do. Closing the gap needs a way to report Resume State once the conversation exists,
/// which the Provider seam does not have.
#[derive(Deserialize, Serialize)]
struct ClaudeResumeState {
    session_id: String,
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
    /// How long an interrupt — and each of the task stops that go ahead of it — waits.
    pub(super) interrupt_request: Duration,
}

pub(super) async fn start_claude_session(
    executable: OsString,
    request: ProviderSessionRequest,
    processes: ProcessRegistry,
    timings: ClaudeTimings,
) -> Result<ProviderSessionConnection, ProviderError> {
    // Read before anything is launched: Resume State Suru cannot read fails the startup outright
    // rather than after a discovery the Session will never use.
    let restored = known_session_id(request.resume_state)?;
    // The Selection the Session reports before the user chooses one: the catalog default, which is
    // what the first Turn will spawn under when nothing else was chosen.
    let selection = default_selection(
        executable.clone(),
        processes.clone(),
        timings.control_request,
    )
    .await?;
    // A restored Session keeps the identifier its conversation is filed under, because that is
    // what the CLI kept the work behind.
    let (provider_session_id, next_spawn) = match restored {
        Some(session_id) => (session_id, ProviderSessionSpawn::Resume),
        None => (uuid::Uuid::new_v4().to_string(), ProviderSessionSpawn::Mint),
    };
    let resume_state = ProviderResumeState::new(
        serde_json::to_value(ClaudeResumeState {
            session_id: provider_session_id.clone(),
        })
        .expect("Claude Resume State serialization is infallible"),
    );

    let (conversation, messages) = tokio::sync::mpsc::unbounded_channel();
    let turn = TurnInFlight::new();
    let events = provider_events(messages, turn.clone());
    let session = Arc::new(ClaudeSession {
        executable,
        processes,
        workspace: request.workspace,
        provider_session_id,
        conversation,
        child: Mutex::new(ChildSlot {
            running: None,
            next_spawn,
        }),
        turn,
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

/// The provider session `resume_state` names, or nothing when the Suru Session has never reached
/// Claude. Resume State Suru cannot read is a failure rather than a reason to start over: the
/// Session it belongs to has a conversation behind it that minting a fresh identifier would
/// abandon while looking as though nothing was lost.
fn known_session_id(
    resume_state: Option<ProviderResumeState>,
) -> Result<Option<String>, ProviderError> {
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
    Ok(Some(state.session_id))
}

/// The Agent Selection a Session with none chosen runs under: the catalog's default row with its
/// default Model Options, asked of the CLI the same way the catalog is.
async fn default_selection(
    executable: OsString,
    processes: ProcessRegistry,
    control_request_timeout: Duration,
) -> Result<AgentSelection, ProviderError> {
    const CONTEXT: &str = "Claude Session startup failed";
    let models = discover_claude_models(executable, processes, control_request_timeout)
        .await
        .map_err(|error| claude_error_context(CONTEXT, error))?;
    models
        .iter()
        .find(|descriptor| descriptor.is_default)
        .map(ModelDescriptor::default_agent_selection)
        .ok_or_else(|| claude_error(format!("{CONTEXT}: the Model catalog offers no default")))
}

struct ClaudeSession {
    executable: OsString,
    processes: ProcessRegistry,
    workspace: PathBuf,
    /// The provider-session UUID Suru minted, which the child is spawned under.
    provider_session_id: String,
    /// Where every child this Session spawns delivers its conversation, so the Session's event
    /// stream outlives any one process.
    conversation: ConversationSink,
    /// The child the Session's Turns run on, and how the next one addresses the conversation.
    child: Mutex<ChildSlot>,
    /// What the Session and the projection of its conversation agree on about the Turn in flight.
    turn: Arc<TurnInFlight>,
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
}

impl ClaudeChild {
    /// Stops the process this child runs on, leaving the conversation it ran on the CLI's disk for
    /// the next child to resume. An intended stop like this one publishes no failure of its own,
    /// so the Session's event stream carries on into the child that replaces it.
    async fn stop(&self) -> Result<(), ProviderError> {
        self.process.begin_shutdown();
        self.transport.close().await;
        self.process.wait_until_stopped().await
    }
}

impl ClaudeSession {
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
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Turn startup failed";
            let mut slot = self.child.lock().await;
            if self.shutdown_started.load(Ordering::Acquire) {
                return Err(claude_error("Claude Session is shutting down"));
            }
            // The Model and its Options are spawn-time flags, so a Turn selected under different
            // ones needs a child of its own; one selected under the flags the running child
            // carries needs nothing.
            if slot
                .running
                .as_ref()
                .is_none_or(|child| child.selection != input.selection)
            {
                // The Selection is lowered onto flags before anything is torn down, so one the CLI
                // has no flags for leaves the Session running on the child it had.
                let args =
                    spawn_args(&self.provider_session_id, &input.selection, slot.next_spawn)?;
                if let Some(previous) = slot.running.take() {
                    previous
                        .stop()
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
                    Some(self.workspace.clone()),
                    Some(self.conversation.clone()),
                    self.processes.clone(),
                )
                .await
                .map_err(|error| claude_error_context(launch_context, error))?;
                // The conversation is the CLI's to keep from here on, so every later child resumes
                // it rather than asking for it to be minted again.
                slot.next_spawn = ProviderSessionSpawn::Resume;
                slot.running = Some(ClaudeChild {
                    transport,
                    process,
                    selection: input.selection.clone(),
                });
            }
            let child = slot
                .running
                .as_ref()
                .expect("a Turn runs on the child that was just spawned for it");
            self.turn.begin_turn();
            child
                .transport
                .send(&UserMessageEnvelope::text(&input.prompt))
                .await
                .map_err(|error| {
                    // The Prompt never reached the CLI, so the Turn it would have begun is not
                    // running and is owed nothing.
                    self.turn.abandon_turn();
                    claude_error_context(CONTEXT, error)
                })
        })
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            const CONTEXT: &str = "Claude Turn steering failed";
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
                .send(&UserMessageEnvelope::text(&input.prompt))
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
                .map_err(|error| {
                    // An interrupt Suru could not deliver fails the Turn, so the Turn is over
                    // whatever the loop does next: nothing the CLI still owes it is its to settle,
                    // and a later steer must not join a Turn Suru has already closed.
                    self.turn.abandon_turn();
                    claude_error_context(CONTEXT, error)
                })
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.shutdown_started.store(true, Ordering::Release);
            let slot = self.child.lock().await;
            let Some(child) = slot.running.as_ref() else {
                return Ok(());
            };
            child.stop().await
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
/// addresses and the full-auto posture every Claude Session launches with.
fn spawn_args(
    provider_session_id: &str,
    selection: &AgentSelection,
    spawn: ProviderSessionSpawn,
) -> Result<Vec<OsString>, ProviderError> {
    debug_assert_eq!(selection.provider.as_str(), CLAUDE_PROVIDER_ID);
    let mut args: Vec<OsString> = [
        "--include-partial-messages",
        "--dangerously-skip-permissions",
        spawn.identity_flag(),
        provider_session_id,
        "--model",
        selection.model.as_str(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut effort = None;
    for option in &selection.options {
        let ModelOptionValue::Select { choice } = &option.value else {
            return Err(ProviderError::selection_rejected(format!(
                "Claude does not support toggle Model Option `{}`",
                option.id
            )));
        };
        match option.id.as_str() {
            REASONING_EFFORT_OPTION_ID if effort.is_none() => {
                effort = Some(choice.as_str().to_owned());
            }
            REASONING_EFFORT_OPTION_ID => {
                return Err(ProviderError::selection_rejected(format!(
                    "Claude Model Option `{}` was selected more than once",
                    option.id
                )));
            }
            _ => {
                return Err(ProviderError::selection_rejected(format!(
                    "Claude does not support Model Option `{}`",
                    option.id
                )));
            }
        }
    }
    if let Some(effort) = effort {
        args.push(OsString::from("--effort"));
        args.push(OsString::from(effort));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::json;

    use super::{ProviderSessionSpawn, known_session_id, spawn_args};
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
            super::{ChildSlot, ClaudeSession, ProviderSessionSpawn, TurnInFlight},
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
                executable: executable.into(),
                processes,
                workspace: directory.path().to_owned(),
                provider_session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
                conversation,
                child: Mutex::new(ChildSlot {
                    running: None,
                    next_spawn: ProviderSessionSpawn::Mint,
                }),
                turn: TurnInFlight::new(),
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
                    prompt: "Answer in French instead".to_owned(),
                })
                .await
                .expect_err("a Session with no Turn running has none to steer");
            assert!(
                error.to_string().contains("no active Turn to steer"),
                "{error}"
            );

            session
                .start_turn(ProviderTurnInput {
                    prompt: "Say hello".to_owned(),
                    selection: selection(Vec::new()),
                })
                .await
                .expect("the first Turn spawns the child");
            session
                .steer_turn(ProviderSteerInput {
                    prompt: "Answer in French instead".to_owned(),
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
                    prompt: "Say hello".to_owned(),
                    selection: selection(Vec::new()),
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
                    prompt: "Too late".to_owned(),
                    selection: selection(Vec::new()),
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
        )
        .expect("a reasoning-effort selection lowers");
        assert_eq!(
            args,
            [
                "--include-partial-messages",
                "--dangerously-skip-permissions",
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
        )
        .expect("a resuming spawn lowers");
        assert_eq!(
            args[2..4],
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
        let args = spawn_args("id", &selection(Vec::new()), ProviderSessionSpawn::Mint)
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
        )
        .expect_err("a duplicated Model Option is a rejected selection");
        assert!(error.is_selection_rejected());
    }

    #[test]
    fn a_session_that_never_reached_claude_mints_a_conversation_of_its_own() {
        assert_eq!(
            known_session_id(None).expect("no Resume State is not a failure"),
            None
        );
    }

    #[test]
    fn the_provider_session_identifier_survives_a_round_trip_through_resume_state() {
        let state = ProviderResumeState::new(json!({
            "session_id": "11111111-2222-3333-4444-555555555555",
        }));
        assert_eq!(
            known_session_id(Some(state)).expect("the Resume State is readable"),
            Some("11111111-2222-3333-4444-555555555555".to_owned())
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
            let error = known_session_id(Some(ProviderResumeState::new(unusable.clone())))
                .expect_err("unusable Resume State fails the Session startup");
            assert!(
                error.to_string().contains("Claude Resume State is invalid"),
                "{unusable} reports what is wrong with it: {error}"
            );
        }
    }
}
