//! Settling Turns and the streams they leave in flight.

use std::collections::HashMap;

use anyhow::anyhow;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentId, AgentIdentity, MessageRole, MessageStatus,
    PromptDelivery, PromptStatus, SessionChange, SessionId, SessionSnapshot, SessionUpdate, Turn,
    TurnId, TurnStatus,
};

use super::{
    SessionStore,
    output::command_output_changes,
    projection::active_turn_id,
    prompts::{
        DeliveredTurn, append_steer_delivery_changes, earliest_pending_prompt,
        prepare_prompt_delivery,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterruptTurnError {
    SessionNotFound,
    TurnNotFound,
    TurnNotActive,
    ProviderFailure(String),
}

pub(crate) enum ProviderTurnOutcome {
    Completed,
    Failed {
        trailing_output: TrailingCommandOutput,
        message: String,
    },
    Interrupted {
        trailing_output: TrailingCommandOutput,
    },
}

/// The authoritative decision made for the next queued Prompt immediately
/// before the settle commit would deliver it. Keeping the decision explicit
/// lets the Provider actor perform asynchronous Skill Catalog validation while
/// the Session store still settles the old Turn and either starts or fails the
/// queued Turn atomically for every attached client.
pub(crate) enum QueuedPromptDisposition {
    Deliver {
        prompt_id: crate::protocol::PromptId,
    },
    Fail {
        prompt_id: crate::protocol::PromptId,
        message: String,
    },
    LeavePending,
}

/// The unterminated line each in-flight command's normalizer held back when its
/// Turn settled, keyed by the command Activity it belongs to. Only the Provider
/// actor holds those normalizers, while which streams are still in flight is the
/// store's own knowledge, so a settle path that cannot reach the actor carries
/// an empty one and still settles every stream the Turn left open.
pub(crate) type TrailingCommandOutput = HashMap<ActivityId, NormalizedText>;

impl SessionStore {
    pub(crate) fn fail_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let mut changes = settle_in_flight_changes(&record.snapshot, turn_id, trailing_output);
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
                settled_at: None,
            },
        ]);
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)
    }

    pub(crate) fn finish_provider_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        agent_id: AgentId,
        outcome: ProviderTurnOutcome,
        queued_prompt_disposition: QueuedPromptDisposition,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (trailing_output, failure_message, status) = match outcome {
            ProviderTurnOutcome::Completed => {
                (TrailingCommandOutput::new(), None, TurnStatus::Completed)
            }
            ProviderTurnOutcome::Failed {
                trailing_output,
                message,
            } => (trailing_output, Some(message), TurnStatus::Failed),
            ProviderTurnOutcome::Interrupted { trailing_output } => {
                (trailing_output, None, TurnStatus::Interrupted)
            }
        };
        let (pending_steers, next_queued_prompt, next_agent, settle_changes) = {
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
                let salvaged = settle_in_flight_changes(&record.snapshot, turn_id, trailing_output);
                if salvaged.is_empty() {
                    return Ok(None);
                }
                let updated_at = state.next_timestamp();
                let record = state
                    .sessions
                    .get_mut(&session_id)
                    .expect("Session existence was checked while holding the store lock");
                record.commit(&self.storage, session_id, salvaged, updated_at)?;
                return Ok(None);
            }
            let mut pending_steers = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                })
                .cloned()
                .collect::<Vec<_>>();
            pending_steers.sort_unstable_by_key(|prompt| prompt.admission_order);
            let next_queued_prompt =
                earliest_pending_prompt(&record.snapshot.prompts, PromptDelivery::Queue).cloned();
            let next_agent = record
                .snapshot
                .session
                .agent_selection
                .clone()
                .map(|selection| AgentIdentity {
                    agent: agent_id,
                    selection,
                });
            (
                pending_steers,
                next_queued_prompt,
                next_agent,
                settle_in_flight_changes(&record.snapshot, turn_id, trailing_output),
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

        let next_turn = match (next_queued_prompt, queued_prompt_disposition) {
            (Some(prompt), QueuedPromptDisposition::Deliver { prompt_id })
                if prompt.id == prompt_id =>
            {
                let (delivered, delivery_changes) =
                    prepare_prompt_delivery(prompt, next_agent, TurnStatus::Active);
                changes.extend(delivery_changes);
                Some(delivered)
            }
            (Some(prompt), QueuedPromptDisposition::Fail { prompt_id, message })
                if prompt.id == prompt_id =>
            {
                let (delivered, delivery_changes) =
                    prepare_prompt_delivery(prompt, next_agent, TurnStatus::Failed);
                changes.extend(delivery_changes);
                changes.push(SessionChange::ActivityAdded {
                    activity: Activity::Error {
                        id: ActivityId::new(),
                        turn_id: delivered.turn_id,
                        text: message,
                    },
                });
                None
            }
            (None, _) | (Some(_), _) => None,
        };

        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)?;
        for prompt in pending_steers {
            record.steer_targets.remove(&prompt.id);
        }
        Ok(next_turn)
    }

    pub(crate) fn interrupt_target(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Turn, InterruptTurnError> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let turn = state
            .sessions
            .get(&session_id)
            .ok_or(InterruptTurnError::SessionNotFound)?
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or(InterruptTurnError::TurnNotFound)?
            .clone();
        if turn.status == TurnStatus::Interrupted {
            return Ok(turn);
        }
        if turn.status != TurnStatus::Active {
            return Err(InterruptTurnError::TurnNotActive);
        }
        Ok(turn)
    }
}

/// The changes that settle everything a Turn left in flight: its streaming Agent
/// Message completes, and each command, file-change, or Reasoning Activity still
/// Active fails, every command first storing the trailing output its normalizer
/// flushed. Reading the in-flight set from the Session's own snapshot rather
/// than from the caller keeps every settle path equivalent, including the ones
/// that never reach the Provider actor holding that Turn. A Turn that completes
/// normally has nothing in flight — a Provider that leaves a stream open is
/// refused its completion — so this settles nothing on that path.
pub(super) fn settle_in_flight_changes(
    snapshot: &SessionSnapshot,
    turn_id: TurnId,
    mut trailing_output: TrailingCommandOutput,
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
            Activity::Subagent {
                id,
                status: ActivityStatus::Active,
                ..
            } => changes.push(SessionChange::SubagentStatusChanged {
                activity_id: *id,
                status: ActivityStatus::Failed,
                // As with Reasoning: the Provider never reported this
                // Subagent settling, so there is no duration to record.
                duration_ms: None,
            }),
            _ => {}
        }
    }
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
            session: Session {
                id: SessionId::new(),
                workspace: Workspace {
                    path: PathBuf::from("/workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Active,
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
            }],
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

        let changes = settle_in_flight_changes(&snapshot, turn_id, TrailingCommandOutput::new());

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

        let changes = settle_in_flight_changes(&snapshot, turn_id, TrailingCommandOutput::new());

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
        );

        assert!(
            changes.is_empty(),
            "a Turn with nothing in flight settles nothing: {changes:?}"
        );
    }
}
