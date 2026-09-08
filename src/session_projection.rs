//! Shared projection rules for authoritative and client-side Session state.

use anyhow::{Result, bail};

use crate::protocol::{
    Activity, ActivityId, ActivityStatus, MessageRole, MessageStatus, PromptDelivery, PromptStatus,
    SessionChange, SessionSnapshot, SessionUpdate, TranscriptItem, TurnStatus,
};

/// Applies `update` to `snapshot` in place. On error the snapshot may hold a
/// partially applied update and must be discarded by the caller; every current
/// caller either replaces the snapshot wholesale or treats the error as fatal.
/// Callers that need atomicity clone before applying.
pub(crate) fn apply_update(snapshot: &mut SessionSnapshot, update: &SessionUpdate) -> Result<()> {
    if snapshot.session.id != update.session_id {
        bail!("Session update targeted a different Session");
    }
    if !update.revision.immediately_follows(snapshot.revision) {
        bail!("Session update revision is not monotonic");
    }

    let next = snapshot;
    for change in &update.changes {
        match change {
            SessionChange::AgentSelectionChanged { selection } => {
                next.session.agent_selection = Some(selection.clone());
            }
            SessionChange::AgentSelectionAvailabilityChanged { availability } => {
                next.session.agent_selection_availability = *availability;
            }
            SessionChange::PromptAdded { prompt } => {
                if next.prompts.iter().any(|existing| existing.id == prompt.id) {
                    bail!("Session update reused a Prompt identity");
                }
                if prompt.admission_order.0 == 0
                    || next
                        .prompts
                        .iter()
                        .any(|existing| existing.admission_order == prompt.admission_order)
                {
                    bail!("Session update reused an invalid Prompt admission order");
                }
                next.prompts.push(prompt.clone());
            }
            SessionChange::PromptDeliveryChanged {
                prompt_id,
                delivery,
            } => {
                let Some(prompt) = next
                    .prompts
                    .iter_mut()
                    .find(|prompt| prompt.id == *prompt_id)
                else {
                    bail!("Session update referenced an unknown Prompt");
                };
                if prompt.status != PromptStatus::Pending
                    || prompt.delivery != PromptDelivery::Queue
                    || *delivery != PromptDelivery::Steer
                {
                    bail!("Session update contained an invalid Prompt promotion");
                }
                prompt.delivery = *delivery;
            }
            SessionChange::PromptStatusChanged { prompt_id, status } => {
                let Some(prompt) = next
                    .prompts
                    .iter_mut()
                    .find(|prompt| prompt.id == *prompt_id)
                else {
                    bail!("Session update referenced an unknown Prompt");
                };
                if prompt.status != PromptStatus::Pending
                    || !matches!(
                        status,
                        PromptStatus::Delivered | PromptStatus::Failed | PromptStatus::Cancelled
                    )
                {
                    bail!("Session update contained an invalid Prompt status transition");
                }
                prompt.status = *status;
            }
            SessionChange::ContextFillChanged { context_fill } => {
                next.session.context_fill = *context_fill;
            }
            SessionChange::TurnAdded { turn } => {
                if !turn.has_valid_cost_attribution() {
                    bail!("Session update added a Turn with a Cost lacking exactly one Cost Basis");
                }
                if let Some(prompt_id) = turn.prompt_id
                    && !next.prompts.iter().any(|prompt| prompt.id == prompt_id)
                {
                    bail!("Session update referenced an unknown Prompt");
                }
                if next.turns.iter().any(|existing| existing.id == turn.id) {
                    bail!("Session update reused a Turn identity");
                }
                let previous_model = next
                    .turns
                    .last()
                    .and_then(|turn| turn.agent.as_ref())
                    .map(|agent| (&agent.selection.provider, &agent.selection.model));
                let new_model = turn
                    .agent
                    .as_ref()
                    .map(|agent| (&agent.selection.provider, &agent.selection.model));
                if previous_model != new_model {
                    next.session.context_fill = None;
                }
                next.turns.push(turn.clone());
            }
            SessionChange::TurnAgentChanged { turn_id, agent } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status != TurnStatus::Active {
                    bail!("Session update changed the Agent on a terminal Turn");
                }
                let Some(current) = turn.agent.as_ref() else {
                    bail!("Session update changed the Agent on an unbound Turn");
                };
                if current.agent != agent.agent
                    || current.selection.provider != agent.selection.provider
                {
                    bail!("Session update changed the Provider identity of an active Turn");
                }
                if current.selection.model != agent.selection.model {
                    next.session.context_fill = None;
                }
                turn.agent = Some(agent.clone());
            }
            SessionChange::TurnUsageChanged {
                turn_id,
                usage,
                cost,
                cost_basis,
            } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status != TurnStatus::Active {
                    bail!("Session update recorded Usage on a terminal Turn");
                }
                if cost.is_some() != cost_basis.is_some() {
                    bail!("Session update recorded a Cost without exactly one Cost Basis");
                }
                turn.usage = Some(usage.clone());
                turn.cost = *cost;
                turn.cost_basis = *cost_basis;
            }
            SessionChange::SubagentQuestionnairesChanged {
                subagent_questionnaires,
            } => {
                next.subagent_questionnaires
                    .clone_from(subagent_questionnaires);
            }
            SessionChange::SubagentUsageChanged { subagent_usage } => {
                // The reading arrives whole, derived by the one party that
                // can see across Sessions, so applying it is taking it as
                // given rather than adding anything up.
                next.subagent_usage = *subagent_usage;
            }
            SessionChange::SessionWorkingChanged { working_since } => {
                next.session.working_since = *working_since;
            }
            SessionChange::MessageAdded { message } => {
                if !next.turns.iter().any(|turn| turn.id == message.turn_id) {
                    bail!("Session update referenced an unknown Turn");
                }
                if next
                    .messages
                    .iter()
                    .any(|existing| existing.id == message.id)
                {
                    bail!("Session update reused a Message identity");
                }
                if message.role == MessageRole::User && message.status != MessageStatus::Completed {
                    bail!("User Messages cannot stream");
                }
                if message.truncated {
                    bail!("Session update added a Message outside its initial state");
                }
                next.messages.push(message.clone());
                next.transcript.push(TranscriptItem::Message {
                    message_id: message.id,
                });
            }
            SessionChange::MessageContentAppended {
                message_id,
                content,
            } => {
                let Some(message) = next
                    .messages
                    .iter_mut()
                    .find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if message.role != MessageRole::Agent || message.status != MessageStatus::Streaming
                {
                    bail!("Session update can only append to a streaming Agent Message");
                }
                if message.truncated {
                    bail!("Session update appended content past the cap that truncated a Message");
                }
                message.content.push_str(content);
            }
            SessionChange::MessageTruncated { message_id } => {
                let Some(message) = next
                    .messages
                    .iter_mut()
                    .find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if message.role != MessageRole::Agent || message.status != MessageStatus::Streaming
                {
                    bail!("Session update can only truncate a streaming Agent Message");
                }
                message.truncated = true;
            }
            SessionChange::MessageCompleted { message_id } => {
                let Some(message) = next
                    .messages
                    .iter_mut()
                    .find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if message.role != MessageRole::Agent || message.status != MessageStatus::Streaming
                {
                    bail!("Session update can only complete a streaming Agent Message");
                }
                message.status = MessageStatus::Completed;
            }
            SessionChange::QuestionnaireAccepted { activity_id } => {
                let Some(Activity::Questionnaire {
                    outcome, turn_id, ..
                }) = next.activities.iter_mut().find(|a| a.id() == *activity_id)
                else {
                    bail!("Unknown Questionnaire Activity");
                };
                if !outcome.is_answerable()
                    || !next.turns.iter().any(|t| {
                        t.id == *turn_id && t.status == crate::protocol::TurnStatus::Active
                    })
                {
                    bail!("Questionnaire is unavailable");
                }
                *outcome = crate::protocol::QuestionnaireOutcome::Submitting;
            }
            SessionChange::QuestionnaireSettled {
                activity_id,
                outcome,
                answer,
            } => {
                let Some(Activity::Questionnaire {
                    outcome: current,
                    answer: stored,
                    ..
                }) = next.activities.iter_mut().find(|a| a.id() == *activity_id)
                else {
                    bail!("Unknown Questionnaire Activity");
                };
                if !current.is_live()
                    || (*outcome == crate::protocol::QuestionnaireOutcome::SubmissionRejected
                        && (*current != crate::protocol::QuestionnaireOutcome::Submitting
                            || answer.is_some()))
                    || matches!(
                        outcome,
                        crate::protocol::QuestionnaireOutcome::Pending
                            | crate::protocol::QuestionnaireOutcome::Submitting
                    )
                {
                    bail!("Questionnaire is already unavailable");
                }
                *current = *outcome;
                *stored = answer.clone();
            }
            SessionChange::ActivityAdded { activity } => {
                if !next.turns.iter().any(|turn| turn.id == activity.turn_id()) {
                    bail!("Session update referenced an unknown Turn");
                }
                if next
                    .activities
                    .iter()
                    .any(|existing| existing.id() == activity.id())
                {
                    bail!("Session update reused an Activity identity");
                }
                if matches!(
                    activity,
                    Activity::Command {
                        status,
                        output,
                        output_truncated,
                        exit_status,
                        ..
                    } if *status != ActivityStatus::Active
                        || !output.is_empty()
                        || *output_truncated
                        || exit_status.is_some()
                ) {
                    bail!("Session update added a command Activity outside its initial state");
                }
                if matches!(
                    activity,
                    Activity::FileChange { status, .. }
                        if *status != ActivityStatus::Active
                ) {
                    bail!("Session update added a file-change Activity outside its initial state");
                }
                if matches!(
                    activity,
                    Activity::Reasoning {
                        status,
                        title,
                        content,
                        content_truncated,
                        duration_ms,
                        ..
                    } if *status != ActivityStatus::Active
                        || title.is_some()
                        || !content.is_empty()
                        || *content_truncated
                        || duration_ms.is_some()
                ) {
                    bail!("Session update added a Reasoning Activity outside its initial state");
                }
                if matches!(
                    activity,
                    Activity::Subagent {
                        status,
                        duration_ms,
                        ..
                    } if *status != ActivityStatus::Active || duration_ms.is_some()
                ) {
                    bail!("Session update added a Subagent Activity outside its initial state");
                }
                next.activities.push(activity.clone());
                next.transcript.push(TranscriptItem::Activity {
                    activity_id: activity.id(),
                });
            }
            SessionChange::CommandOutputAppended {
                activity_id,
                content,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::Command {
                    status,
                    output,
                    output_truncated,
                    ..
                } = activity
                else {
                    bail!("Session update appended command output to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended output to a terminal command Activity");
                }
                if *output_truncated {
                    bail!("Session update appended output past the cap that truncated a command");
                }
                output.push_str(content);
            }
            SessionChange::CommandOutputTruncated { activity_id } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::Command {
                    status,
                    output_truncated,
                    ..
                } = activity
                else {
                    bail!("Session update truncated the output of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the output of a terminal command Activity");
                }
                *output_truncated = true;
            }
            SessionChange::CommandStatusChanged {
                activity_id,
                status,
                exit_status,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::Command {
                    status: current_status,
                    exit_status: current_exit_status,
                    ..
                } = activity
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active
                    || !matches!(status, ActivityStatus::Completed | ActivityStatus::Failed)
                {
                    bail!("Session update contained an invalid command Activity status transition");
                }
                *current_status = *status;
                *current_exit_status = *exit_status;
            }
            SessionChange::FileChangeUpdated {
                activity_id,
                changes,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::FileChange {
                    status,
                    changes: current_changes,
                    ..
                } = activity
                else {
                    bail!("Session update changed paths on a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update changed paths on a terminal file-change Activity");
                }
                *current_changes = changes.clone();
            }
            SessionChange::FileChangeStatusChanged {
                activity_id,
                status,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::FileChange {
                    status: current_status,
                    ..
                } = activity
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active
                    || !matches!(status, ActivityStatus::Completed | ActivityStatus::Failed)
                {
                    bail!(
                        "Session update contained an invalid file-change Activity status transition"
                    );
                }
                *current_status = *status;
            }
            SessionChange::ReasoningTitleChanged { activity_id, title } => {
                let Some(Activity::Reasoning {
                    status,
                    title: current_title,
                    ..
                }) = reasoning_activity(next, activity_id)?
                else {
                    bail!("Session update titled a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update titled a terminal Reasoning Activity");
                }
                *current_title = Some(title.clone());
            }
            SessionChange::ReasoningContentAppended {
                activity_id,
                content,
            } => {
                let Some(Activity::Reasoning {
                    status,
                    content: current_content,
                    content_truncated,
                    ..
                }) = reasoning_activity(next, activity_id)?
                else {
                    bail!("Session update appended Reasoning to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended content to a terminal Reasoning Activity");
                }
                if *content_truncated {
                    bail!("Session update appended content past the cap that truncated Reasoning");
                }
                current_content.push_str(content);
            }
            SessionChange::ReasoningContentTruncated { activity_id } => {
                let Some(Activity::Reasoning {
                    status,
                    content_truncated,
                    ..
                }) = reasoning_activity(next, activity_id)?
                else {
                    bail!("Session update truncated the content of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the content of a terminal Reasoning Activity");
                }
                *content_truncated = true;
            }
            SessionChange::ReasoningStatusChanged {
                activity_id,
                status,
                duration_ms,
            } => {
                let Some(Activity::Reasoning {
                    status: current_status,
                    duration_ms: current_duration_ms,
                    ..
                }) = reasoning_activity(next, activity_id)?
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active
                    || !matches!(status, ActivityStatus::Completed | ActivityStatus::Failed)
                {
                    bail!(
                        "Session update contained an invalid Reasoning Activity status transition"
                    );
                }
                *current_status = *status;
                *current_duration_ms = *duration_ms;
            }
            SessionChange::SubagentDescriptionChanged {
                activity_id,
                description,
            } => {
                let Some(Activity::Subagent {
                    status,
                    description: current_description,
                    ..
                }) = subagent_activity(next, activity_id)?
                else {
                    bail!("Session update described a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update described a terminal Subagent Activity");
                }
                *current_description = description.clone();
            }
            SessionChange::SubagentStatusChanged {
                activity_id,
                status,
                duration_ms,
            } => {
                let Some(Activity::Subagent {
                    status: current_status,
                    duration_ms: current_duration_ms,
                    ..
                }) = subagent_activity(next, activity_id)?
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active
                    || !matches!(
                        status,
                        ActivityStatus::Completed
                            | ActivityStatus::Failed
                            | ActivityStatus::Interrupted
                    )
                {
                    bail!(
                        "Session update contained an invalid Subagent Activity status transition"
                    );
                }
                *current_status = *status;
                *current_duration_ms = *duration_ms;
            }
            SessionChange::TurnStatusChanged {
                turn_id,
                status,
                settled_at,
            } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status.is_terminal() || !status.is_terminal() {
                    bail!("Session update contained an invalid Turn status transition");
                }
                turn.status = *status;
                turn.settled_at = *settled_at;
            }
            SessionChange::SessionStatusChanged { status } => next.session.status = *status,
        }
    }
    next.revision = update.revision;
    Ok(())
}

/// Resolves the Activity a Reasoning change names, failing when the Session
/// has no such Activity so every Reasoning arm reports the same miss the same
/// way and is left to check only that the Activity is the kind it can act on.
fn reasoning_activity<'a>(
    snapshot: &'a mut SessionSnapshot,
    activity_id: &ActivityId,
) -> Result<Option<&'a mut Activity>> {
    let Some(activity) = snapshot
        .activities
        .iter_mut()
        .find(|activity| activity.id() == *activity_id)
    else {
        bail!("Session update referenced an unknown Activity");
    };
    Ok(matches!(activity, Activity::Reasoning { .. }).then_some(activity))
}

/// Resolves the Activity a Subagent change names, on the same terms as
/// [`reasoning_activity`].
fn subagent_activity<'a>(
    snapshot: &'a mut SessionSnapshot,
    activity_id: &ActivityId,
) -> Result<Option<&'a mut Activity>> {
    let Some(activity) = snapshot
        .activities
        .iter_mut()
        .find(|activity| activity.id() == *activity_id)
    else {
        bail!("Session update referenced an unknown Activity");
    };
    Ok(matches!(activity, Activity::Subagent { .. }).then_some(activity))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::protocol::{
        Cost, CostBasis, ModelAvailability, Prompt, PromptId, PromptOrder, Session, SessionId,
        SessionRevision, SessionStatus, SessionTimestamp, Turn, TurnId, Usage, Workspace,
    };

    use super::*;

    #[test]
    fn a_client_reads_turn_timing_and_usage_off_the_changes_the_server_committed() {
        let session_id = SessionId::new();
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let mut snapshot = SessionSnapshot {
            session: Session {
                context_fill: None,
                id: session_id,
                workspace: Workspace {
                    path: PathBuf::from("/workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
                working_since: None,
                parent: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: vec![Prompt {
                id: prompt_id,
                text: "Work on this".to_owned(),
                skill_invocations: Vec::new(),
                delivery: PromptDelivery::Steer,
                admission_order: PromptOrder::INITIAL,
                status: PromptStatus::Pending,
            }],
            turns: Vec::new(),
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            subagent_questionnaires: Vec::new(),
            subagent_usage: None,
        };

        apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes: vec![SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id: Some(prompt_id),
                        agent: None,
                        status: TurnStatus::Active,
                        started_at: Some(SessionTimestamp(1_755_000_000_000)),
                        settled_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                    },
                }],
            },
        )
        .expect("a delivered Turn joins the Session");
        assert_eq!(
            snapshot.turns[0].started_at,
            Some(SessionTimestamp(1_755_000_000_000))
        );
        assert_eq!(snapshot.turns[0].settled_at, None);

        apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(3),
                changes: vec![SessionChange::TurnUsageChanged {
                    turn_id,
                    usage: Usage {
                        fresh_input_tokens: Some(1_200),
                        output_tokens: Some(900),
                        ..Usage::default()
                    },
                    cost: Cost::from_usd(0.03),
                    cost_basis: Some(CostBasis::Reported),
                }],
            },
        )
        .expect("the Provider's Usage lands on the active Turn");
        assert_eq!(
            snapshot.turns[0]
                .usage
                .as_ref()
                .and_then(Usage::blended_tokens),
            Some(2_100)
        );
        assert_eq!(snapshot.turns[0].cost, Cost::from_usd(0.03));
        assert_eq!(snapshot.turns[0].cost_basis, Some(CostBasis::Reported));

        apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(4),
                changes: vec![SessionChange::TurnStatusChanged {
                    turn_id,
                    status: TurnStatus::Completed,
                    settled_at: Some(SessionTimestamp(1_755_000_004_200)),
                }],
            },
        )
        .expect("the Turn settles");
        assert_eq!(
            snapshot.turns[0].started_at,
            Some(SessionTimestamp(1_755_000_000_000)),
            "settling a Turn leaves when it started alone"
        );
        assert_eq!(
            snapshot.turns[0].settled_at,
            Some(SessionTimestamp(1_755_000_004_200))
        );
    }
}
