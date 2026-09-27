//! Settling Turns and the streams they leave in flight.

use std::collections::HashMap;

use anyhow::anyhow;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentIdentity, ApprovalOutcome, MessageRole,
    MessageStatus, Prompt, PromptDelivery, PromptStatus, QuestionnaireOutcome, SessionChange,
    SessionId, SessionSnapshot, SessionTimestamp, SessionUpdate, Turn, TurnId, TurnStatus,
};

use super::{
    SessionStore, SessionStoreState,
    output::command_output_changes,
    projection::{active_turn_id, undelivered_turn_starts},
    prompts::append_steer_delivery_changes,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterruptSessionError {
    SessionNotFound,
    /// Nothing below the Session is running: no active Turn, no working
    /// Subagent, and no live Watch anywhere in its subtree.
    NothingToInterrupt,
    /// The interrupt named a Subagent's Session whose Provider offers no
    /// per-Subagent stop.
    SubagentStopUnsupported,
    ProviderFailure(String),
    /// The interrupt reached no Provider at all: withdrawing the Prompt the
    /// Session was Working over is a store mutation, and the store refused it.
    Storage(String),
}

/// What one interrupt of a Session did, resolved by the store so every caller
/// acts on the same reading of the same snapshot. Everything the interrupt
/// still has to reach is the Provider's; the one thing the store settles
/// itself, it has settled by the time it answers.
pub(crate) enum InterruptTarget {
    /// The Session's active Turn. The Provider stops the Turn's background
    /// work — its Subagents included — before the loop, in the established
    /// ordering.
    Turn(Box<Turn>),
    /// No Turn is active, but Subagents below the Session still work; the
    /// interrupt stops them all.
    Subagents,
    /// The Session is a Subagent's own, so the interrupt stops that one
    /// Subagent — through the Provider connection its root ancestor owns.
    Subagent { root: SessionId },
    /// Nothing is Working, but the Session is Monitoring: the interrupt asks
    /// the Provider to stop the Watches live in its subtree, and none above
    /// it — through the Provider connection its root ancestor owns, which is
    /// the Session itself for a top-level one. No Turn settles and no Prompt
    /// is withdrawn; the Session goes idle as they settle.
    Watches { root: SessionId },
    /// The Session was Working only because it owed a Turn to a Prompt it had
    /// not delivered, so the interrupt withdrew that Prompt where it stood.
    /// The Prompt is already Cancelled by the time this is returned: the
    /// decision and the mutation are one act under the store lock, so a
    /// delivery that wins the race is seen as a Turn to stop instead.
    WithdrewPrompt(Box<Prompt>),
}

/// What the store sees when it is asked what an interrupt should reach, before
/// anything acts on it. It differs from [`InterruptTarget`] in the one case the
/// store settles itself: an undelivered Prompt is something to withdraw here
/// and something already withdrawn there.
enum InterruptReading {
    Turn(Box<Turn>),
    Subagents,
    Subagent {
        root: SessionId,
    },
    Watches {
        root: SessionId,
    },
    /// No work is under way: the Session is Working only because it owes a
    /// Turn to this Prompt, which it has admitted and not delivered.
    UndeliveredPrompt(Box<Prompt>),
}

pub(crate) enum ProviderTurnOutcome {
    Completed {
        /// A Turn may reach its completion with command streams still open —
        /// a Provider that completed its Turn without settling them, a
        /// Continuation settled early by the next Prompt, or a Subagent's Turn
        /// settled by the Provider's own settle signal. The Turn's settle
        /// closes those streams, and the pending line each one's normalizer
        /// held back still deserves storing.
        trailing_output: TrailingCommandOutput,
    },
    Failed {
        trailing_output: TrailingCommandOutput,
        message: String,
    },
    Interrupted {
        trailing_output: TrailingCommandOutput,
    },
}

/// The unterminated line each in-flight command's normalizer held back when its
/// Turn settled, keyed by the command Activity it belongs to. Only the Provider
/// actor holds those normalizers, while which streams are still in flight is the
/// store's own knowledge, so a settle path that cannot reach the actor carries
/// an empty one and still settles every stream the Turn left open.
pub(crate) type TrailingCommandOutput = HashMap<ActivityId, NormalizedText>;

/// What a settlement says of the Approvals and Questionnaires the Turn still
/// had open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenInterventions {
    /// The Turn ended while the Provider was still there to be answered: the
    /// request simply outlived its Turn.
    TurnEnded,
    /// The process that could have answered is gone — Suru stopped, or is
    /// starting again — so a request still waiting becomes Unavailable and a
    /// decision in delivery becomes DeliveryUncertain, which no restart can
    /// tell from delivered.
    Abandoned,
}

impl OpenInterventions {
    /// The outcome an Approval settles to under this reading, or its own if
    /// it was not open.
    pub(super) fn approval_outcome(self, outcome: ApprovalOutcome) -> ApprovalOutcome {
        match (outcome, self) {
            (
                ApprovalOutcome::Submitting
                | ApprovalOutcome::Pending
                | ApprovalOutcome::SubmissionRejected,
                Self::TurnEnded,
            ) => ApprovalOutcome::TurnEnded,
            (ApprovalOutcome::Submitting, Self::Abandoned) => ApprovalOutcome::DeliveryUncertain,
            (ApprovalOutcome::Pending | ApprovalOutcome::SubmissionRejected, Self::Abandoned) => {
                ApprovalOutcome::Unavailable
            }
            (other, _) => other,
        }
    }

    /// The outcome a Questionnaire settles to under this reading, or its own
    /// if it was not open.
    pub(super) fn questionnaire_outcome(
        self,
        outcome: QuestionnaireOutcome,
    ) -> QuestionnaireOutcome {
        match (outcome, self) {
            (
                QuestionnaireOutcome::Submitting
                | QuestionnaireOutcome::Pending
                | QuestionnaireOutcome::SubmissionRejected,
                Self::TurnEnded,
            ) => QuestionnaireOutcome::TurnEnded,
            (QuestionnaireOutcome::Submitting, Self::Abandoned) => {
                QuestionnaireOutcome::DeliveryUncertain
            }
            (
                QuestionnaireOutcome::Pending | QuestionnaireOutcome::SubmissionRejected,
                Self::Abandoned,
            ) => QuestionnaireOutcome::Unavailable,
            (other, _) => other,
        }
    }
}

impl SessionStore {
    pub(crate) fn fail_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
        interventions: OpenInterventions,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let changes = fail_turn_changes(
            &record.snapshot,
            turn_id,
            trailing_output,
            message,
            None,
            interventions,
        );
        state.commit(&self.storage, session_id, changes)
    }

    /// Begins a Continuation: the one kind of Turn that starts without a
    /// Prompt, begun by Suru itself when Provider output arrives while no Turn
    /// is active — a native loop resumed, or earlier Subagents still owed
    /// output past their Turn's settle (ADR 0015). It settles like any Turn,
    /// so nothing else about settlement knows it exists.
    pub(crate) fn begin_continuation(
        &self,
        session_id: SessionId,
        agent: AgentIdentity,
    ) -> anyhow::Result<TurnId> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)?.is_some() {
                return Err(anyhow!(
                    "A Continuation cannot begin while a Turn is active"
                ));
            }
        }
        let turn_id = TurnId::new();
        state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::TurnAdded {
                turn: Turn {
                    id: turn_id,
                    prompt_id: None,
                    agent: Some(agent),
                    status: TurnStatus::Active,
                    // The commit that lands this Turn stamps when it began.
                    started_at: None,
                    settled_at: None,
                    last_output_at: None,
                    usage: None,
                    cost: None,
                    cost_basis: None,
                    cost_details: None,
                },
            }],
        )?;
        Ok(turn_id)
    }

    /// Settles only this Turn. The Provider actor delivers queued Prompts separately, after
    /// giving any buffered native Continuation its own Turn and interrupt boundary.
    pub(crate) fn finish_provider_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        outcome: ProviderTurnOutcome,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (trailing_output, failure_message, status) = match outcome {
            ProviderTurnOutcome::Completed { trailing_output } => {
                (trailing_output, None, TurnStatus::Completed)
            }
            ProviderTurnOutcome::Failed {
                trailing_output,
                message,
            } => (trailing_output, Some(message), TurnStatus::Failed),
            ProviderTurnOutcome::Interrupted { trailing_output } => {
                (trailing_output, None, TurnStatus::Interrupted)
            }
        };
        let (pending_steers, settle_changes) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)? != Some(turn_id) {
                // The store has moved past this Turn, so another settle path reached it
                // first. Settling a Turn settles its in-flight streams in the same commit,
                // and a Provider actor stays reachable until it has settled the Turn it
                // was running, so the flushed output arriving here should find nothing
                // left to land on. Store what it does still find rather than trusting
                // that and discarding it unseen.
                let salvaged = settle_in_flight_changes(
                    &record.snapshot,
                    turn_id,
                    trailing_output,
                    OpenInterventions::TurnEnded,
                );
                if salvaged.is_empty() {
                    return Ok(());
                }
                state.commit(&self.storage, session_id, salvaged)?;
                return Ok(());
            }
            // A Continuation's pending Prompts owe their own Turns. Keep
            // those admissions out of later prompted Turns' steer sweeps too.
            let settling_continuation = record
                .snapshot
                .turns
                .iter()
                .any(|turn| turn.id == turn_id && turn.is_continuation());
            let mut pending_steers = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    !settling_continuation
                        && prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                        && !record.turn_start_admissions.contains_key(&prompt.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            pending_steers.sort_unstable_by_key(|prompt| prompt.admission_order);
            (
                pending_steers,
                settle_in_flight_changes(
                    &record.snapshot,
                    turn_id,
                    trailing_output,
                    OpenInterventions::TurnEnded,
                ),
            )
        };

        let mut changes = Vec::with_capacity(pending_steers.len() * 2 + settle_changes.len() + 6);
        for prompt in &pending_steers {
            append_steer_delivery_changes(&mut changes, prompt, turn_id);
        }
        changes.extend(settle_changes);
        if let Some(message) = failure_message {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            });
        }
        changes.push(SessionChange::TurnStatusChanged {
            turn_id,
            status,
            settled_at: None,
        });

        state.commit(&self.storage, session_id, changes)?;
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        for prompt in pending_steers {
            record.steer_targets.remove(&prompt.id);
        }
        Ok(())
    }

    /// What one interrupt of a Session should reach, deciding and — where the
    /// answer is a Prompt — acting under the one lock.
    ///
    /// Stopping work comes first: a Session running a Turn, or with Subagents
    /// outliving one, is interrupted the way it always was, and a Session
    /// that is only Monitoring has its Watches stopped. Only a Session
    /// Working solely because it owes a Turn to an undelivered Prompt has that
    /// Prompt withdrawn, and because the delivery that would end that state
    /// takes this same lock, the race resolves one way or the other rather
    /// than into a failure: delivery first leaves a Turn here to stop, and the
    /// withdrawal first leaves the actor a Cancelled Prompt it declines to
    /// deliver.
    pub(crate) fn interrupt_or_withdraw(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptTarget, InterruptSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = match state.interrupt_reading(session_id)? {
            InterruptReading::Turn(turn) => return Ok(InterruptTarget::Turn(turn)),
            InterruptReading::Subagents => return Ok(InterruptTarget::Subagents),
            InterruptReading::Subagent { root } => {
                return Ok(InterruptTarget::Subagent { root });
            }
            InterruptReading::Watches { root } => return Ok(InterruptTarget::Watches { root }),
            InterruptReading::UndeliveredPrompt(prompt) => prompt,
        };
        let prompt_id = prompt.id;
        state
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptStatusChanged {
                    prompt_id,
                    status: PromptStatus::Cancelled,
                }],
            )
            .map_err(|error| InterruptSessionError::Storage(error.to_string()))?;
        let mut withdrawn = *prompt;
        withdrawn.status = PromptStatus::Cancelled;
        Ok(InterruptTarget::WithdrewPrompt(Box::new(withdrawn)))
    }
}

impl SessionStoreState {
    /// The Session whose Provider actor holds `session_id`'s conversation:
    /// its root ancestor. The walk stops where the chain leaves the store: a
    /// Subagent severed from its lineage has no actor left to reach, and the
    /// caller finds nothing to stop under whatever Session the walk ends on.
    fn root_of(&self, session_id: SessionId) -> SessionId {
        let mut root = session_id;
        while let Some(parent) = self
            .sessions
            .get(&root)
            .and_then(|record| record.snapshot.session.parent)
        {
            root = parent;
        }
        root
    }

    /// The reading itself, taken off a locked store so both the question that
    /// only looks and the one that acts ask it of the same state.
    fn interrupt_reading(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptReading, InterruptSessionError> {
        let record = self
            .sessions
            .get(&session_id)
            .ok_or(InterruptSessionError::SessionNotFound)?;
        if record.snapshot.session.parent.is_some() {
            let root = self.root_of(session_id);
            // A Subagent whose own work has settled, with nothing below it
            // Working either, can only be Monitoring: its Watches outlive it,
            // and stopping them — its subtree's, never those of the Sessions
            // above — is the whole interrupt (ADR 0030). Anything still
            // Working is that Subagent to stop.
            let session = &record.snapshot.session;
            if session.working_since.is_none() && session.monitoring_since.is_some() {
                return Ok(InterruptReading::Watches { root });
            }
            return Ok(InterruptReading::Subagent { root });
        }
        if let Some(turn) = record
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.status == TurnStatus::Active)
        {
            return Ok(InterruptReading::Turn(Box::new(turn.clone())));
        }
        if self.subtree_working_since(session_id).is_some() {
            // Work below the Session outranks a Prompt waiting above it: an
            // interrupt reaches what is running, and only a Session Working
            // solely because it owes a Turn has that Prompt withdrawn instead
            // (ADR 0024).
            let subagents_working =
                self.subtree(session_id)
                    .into_iter()
                    .skip(1)
                    .any(|descendant| {
                        self.sessions
                            .get(&descendant)
                            .is_some_and(|record| record.snapshot.session.working_since.is_some())
                    });
            if !subagents_working
                && let Some(prompt) =
                    undelivered_turn_starts(&record.snapshot, &record.turn_start_admissions).next()
            {
                return Ok(InterruptReading::UndeliveredPrompt(Box::new(
                    prompt.clone(),
                )));
            }
            return Ok(InterruptReading::Subagents);
        }
        if record.snapshot.session.monitoring_since.is_some() {
            // Only Watches are left, and nothing a Turn owns: stopping them
            // is the whole interrupt (ADR 0030).
            return Ok(InterruptReading::Watches { root: session_id });
        }
        Err(InterruptSessionError::NothingToInterrupt)
    }
}

/// The changes that settle everything a Turn left in flight: its streaming Agent
/// Message completes, and each command, file-change, or Reasoning Activity still
/// Active fails, every command first storing the trailing output its normalizer
/// flushed. Reading the in-flight set from the Session's own snapshot rather
/// than from the caller keeps every settle path equivalent, including the ones
/// that never reach the Provider actor holding that Turn. A Turn the Provider
/// completes has nothing in flight — a Provider that leaves a stream open is
/// refused its completion — so this settles nothing on that path; a
/// Continuation settled early by the next delivered Prompt is the completion
/// that can still find streams open.
pub(super) fn settle_in_flight_changes(
    snapshot: &SessionSnapshot,
    turn_id: TurnId,
    mut trailing_output: TrailingCommandOutput,
    interventions: OpenInterventions,
) -> Vec<SessionChange> {
    let mut changes = Vec::new();
    changes.extend(
        snapshot
            .messages
            .iter()
            .filter(|message| {
                message.turn_id == turn_id
                    && message.role == MessageRole::Agent
                    && message.status == MessageStatus::Streaming
            })
            .map(|message| SessionChange::MessageCompleted {
                message_id: message.id,
            }),
    );
    for activity in &snapshot.activities {
        if activity.turn_id() != turn_id {
            continue;
        }
        match activity {
            Activity::Approval {
                id,
                outcome:
                    outcome @ (ApprovalOutcome::Pending
                    | ApprovalOutcome::SubmissionRejected
                    | ApprovalOutcome::Submitting),
                ..
            } => changes.push(SessionChange::ApprovalSettled {
                activity_id: *id,
                outcome: interventions.approval_outcome(*outcome),
                decision: None,
            }),
            Activity::Questionnaire {
                id,
                outcome:
                    outcome @ (QuestionnaireOutcome::Pending
                    | QuestionnaireOutcome::SubmissionRejected
                    | QuestionnaireOutcome::Submitting),
                ..
            } => changes.push(SessionChange::QuestionnaireSettled {
                activity_id: *id,
                outcome: interventions.questionnaire_outcome(*outcome),
                answer: None,
            }),
            Activity::Command {
                id,
                status: ActivityStatus::Active,
                ..
            } => {
                if let Some(output) = trailing_output.remove(id) {
                    changes.extend(command_output_changes(*id, output));
                }
                changes.push(SessionChange::CommandStatusChanged {
                    activity_id: *id,
                    status: ActivityStatus::Failed,
                    exit_status: None,
                });
            }
            Activity::FileChange {
                id,
                status: ActivityStatus::Active,
                ..
            } => changes.push(SessionChange::FileChangeStatusChanged {
                activity_id: *id,
                status: ActivityStatus::Failed,
            }),
            Activity::Reasoning {
                id,
                status: ActivityStatus::Active,
                ..
            } => changes.push(SessionChange::ReasoningStatusChanged {
                activity_id: *id,
                status: ActivityStatus::Failed,
                // Only the Provider actor timed the block, and a Turn that
                // settles this way never reported the block finishing, so
                // there is no duration to record.
                duration_ms: None,
            }),
            // A Subagent row is deliberately left standing: a Subagent may
            // outlive the Turn that spawned it (ADR 0015), so its row settles
            // only on the Provider's own settle signal — or when the
            // connection carrying it is torn down, which the orchestrator
            // settles route by route.
            _ => {}
        }
    }
    changes
}

/// The one record of a Turn that failed: every stream it left in flight
/// settled, an Error Activity saying why, and the Turn itself Failed. A live
/// failure leaves `settled_at` to the commit's clock; a Turn found still open
/// at the next start carries the moment it last showed work, since the commit
/// that settles it runs long after the work ended (ADR 0029).
pub(super) fn fail_turn_changes(
    snapshot: &SessionSnapshot,
    turn_id: TurnId,
    trailing_output: TrailingCommandOutput,
    message: String,
    settled_at: Option<SessionTimestamp>,
    interventions: OpenInterventions,
) -> Vec<SessionChange> {
    let mut changes = settle_in_flight_changes(snapshot, turn_id, trailing_output, interventions);
    changes.extend([
        SessionChange::ActivityAdded {
            activity: Activity::Error {
                id: ActivityId::new(),
                turn_id,
                text: message,
            },
        },
        SessionChange::TurnStatusChanged {
            turn_id,
            status: TurnStatus::Failed,
            settled_at,
        },
    ]);
    changes
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::protocol::{
        Message, MessageId, ModelAvailability, PromptId, Session, SessionId, SessionRevision,
        SessionStatus, TranscriptItem, Workspace,
    };

    use super::*;

    fn settling_snapshot(
        turn_id: TurnId,
        activities: Vec<Activity>,
        messages: Vec<Message>,
    ) -> SessionSnapshot {
        SessionSnapshot {
            title: String::new(),
            icon: None,
            session: Session {
                checkout: None,
                context_fill: None,
                id: SessionId::new(),
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: PathBuf::from("/workspace"),
                },
                workspace: Workspace::directory(PathBuf::from("/workspace")),
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                approval_posture: None,
                status: SessionStatus::Active,
                working_since: None,
                monitoring_since: None,
                parent: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: vec![Turn {
                id: turn_id,
                prompt_id: Some(PromptId::new()),
                agent: None,
                status: TurnStatus::Active,
                started_at: None,
                settled_at: None,
                last_output_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
                cost_details: None,
            }],
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            subagent_usage: None,
            total_cost: None,
            transcript: messages
                .iter()
                .map(|message| TranscriptItem::Message {
                    message_id: message.id,
                })
                .chain(activities.iter().map(|activity| TranscriptItem::Activity {
                    activity_id: activity.id(),
                }))
                .collect(),
            messages,
            activities,
        }
    }

    fn command(turn_id: TurnId, status: ActivityStatus) -> Activity {
        Activity::Command {
            id: ActivityId::new(),
            turn_id,
            status,
            command: "report progress".to_owned(),
            cwd: None,
            output: String::new(),
            output_truncated: false,
            exit_status: None,
        }
    }

    #[test]
    fn settling_a_turn_terminates_the_in_flight_streams_its_snapshot_still_shows() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let file_change = Activity::FileChange {
            id: ActivityId::new(),
            turn_id,
            status: ActivityStatus::Active,
            changes: Vec::new(),
        };
        let streaming = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Streaming,
            content: String::new(),
            skill_invocations: Vec::new(),
            truncated: false,
        };
        let snapshot = settling_snapshot(
            turn_id,
            vec![running.clone(), file_change.clone()],
            vec![streaming.clone()],
        );

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::new(),
            OpenInterventions::TurnEnded,
        );

        assert_eq!(
            changes,
            vec![
                SessionChange::MessageCompleted {
                    message_id: streaming.id,
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
                SessionChange::FileChangeStatusChanged {
                    activity_id: file_change.id(),
                    status: ActivityStatus::Failed,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_stores_flushed_command_output_before_the_command_settles() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let snapshot = settling_snapshot(turn_id, vec![running.clone()], Vec::new());

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                running.id(),
                NormalizedText {
                    content: "progress 90%".to_owned(),
                    truncated: false,
                },
            )]),
            OpenInterventions::TurnEnded,
        );

        assert_eq!(
            changes,
            vec![
                SessionChange::CommandOutputAppended {
                    activity_id: running.id(),
                    content: "progress 90%".to_owned(),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_stores_the_truncation_its_flushed_output_reached() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let snapshot = settling_snapshot(turn_id, vec![running.clone()], Vec::new());

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                running.id(),
                NormalizedText {
                    content: "as much as the cap held".to_owned(),
                    truncated: true,
                },
            )]),
            OpenInterventions::TurnEnded,
        );

        assert_eq!(
            changes,
            vec![
                SessionChange::CommandOutputAppended {
                    activity_id: running.id(),
                    content: "as much as the cap held".to_owned(),
                },
                SessionChange::CommandOutputTruncated {
                    activity_id: running.id(),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_fails_the_reasoning_it_left_unfinished_without_a_duration() {
        let turn_id = TurnId::new();
        let reasoning = Activity::Reasoning {
            id: ActivityId::new(),
            turn_id,
            status: ActivityStatus::Active,
            title: Some("Inspecting the seam".to_owned()),
            content: "Reading it.".to_owned(),
            content_truncated: false,
            duration_ms: None,
        };
        let snapshot = settling_snapshot(turn_id, vec![reasoning.clone()], Vec::new());

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::new(),
            OpenInterventions::TurnEnded,
        );

        assert_eq!(
            changes,
            vec![SessionChange::ReasoningStatusChanged {
                activity_id: reasoning.id(),
                status: ActivityStatus::Failed,
                duration_ms: None,
            }]
        );
    }

    #[test]
    fn settling_a_turn_leaves_settled_streams_and_other_turns_alone() {
        let turn_id = TurnId::new();
        let other_turn_id = TurnId::new();
        let settled = command(turn_id, ActivityStatus::Completed);
        let elsewhere = command(other_turn_id, ActivityStatus::Active);
        let completed = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "done".to_owned(),
            skill_invocations: Vec::new(),
            truncated: false,
        };
        let user = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "report progress".to_owned(),
            skill_invocations: Vec::new(),
            truncated: false,
        };
        let snapshot = settling_snapshot(
            turn_id,
            vec![settled.clone(), elsewhere.clone()],
            vec![user, completed],
        );

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                settled.id(),
                NormalizedText {
                    content: "lost".to_owned(),
                    truncated: false,
                },
            )]),
            OpenInterventions::TurnEnded,
        );

        assert!(
            changes.is_empty(),
            "a Turn with nothing in flight settles nothing: {changes:?}"
        );
    }
}
