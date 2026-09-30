//! Shared projection rules for authoritative and client-side Session state.

use anyhow::{Result, bail};

use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AttachmentDescriptor, CostDetails, CostRecord,
    MessageRole, MessageStatus, PromptDelivery, PromptStatus, SessionChange, SessionSnapshot,
    SessionUpdate, TranscriptItem, TurnStatus,
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
            SessionChange::TitleChanged { title, icon } => {
                next.title.clone_from(title);
                next.icon.clone_from(icon);
            }
            SessionChange::AgentSelectionChanged { selection } => {
                next.session.agent_selection = Some(selection.clone());
            }
            SessionChange::AgentSelectionAvailabilityChanged { availability } => {
                next.session.agent_selection_availability = *availability;
            }
            SessionChange::ApprovalPostureChanged { approval_posture } => {
                next.session.approval_posture = *approval_posture;
            }
            SessionChange::AttachmentsDescribed { attachments } => {
                describe_attachments(&mut next.attachments, attachments.iter().cloned());
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
            SessionChange::SubagentAgentChanged { turn_id, agent } => {
                if !next.session.is_subagent() {
                    bail!("Session update observed a Subagent Agent on a root Session");
                }
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if let Some(current) = turn.agent.as_ref()
                    && (current.agent != agent.agent
                        || current.selection.provider != agent.selection.provider)
                {
                    bail!("Session update changed the Provider identity of a Subagent Turn");
                }
                if turn
                    .agent
                    .as_ref()
                    .is_some_and(|current| current.selection.model != agent.selection.model)
                {
                    next.session.context_fill = None;
                }
                turn.agent = Some(agent.clone());
            }
            SessionChange::TurnUsageChanged {
                turn_id,
                usage,
                cost,
                cost_basis,
                cost_coverage,
                cost_is_partial,
                cost_recorded_at,
            } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status != TurnStatus::Active {
                    bail!("Session update recorded Usage on a terminal Turn");
                }
                if cost.is_some() != cost_basis.is_some()
                    || cost.is_some() != cost_coverage.is_some()
                    || cost.is_some() != cost_recorded_at.is_some()
                {
                    bail!("Session update recorded an incomplete Cost attribution");
                }
                turn.usage = Some(usage.clone());
                if let (Some(cost), Some(basis), Some(coverage), Some(recorded_at)) =
                    (cost, cost_basis, cost_coverage, cost_recorded_at)
                {
                    let mut prior = turn
                        .cost_details
                        .as_ref()
                        .map_or_else(Vec::new, |details| details.prior.clone());
                    if let (Some(old_cost), Some(old_basis), Some(old_details)) =
                        (turn.cost, turn.cost_basis, turn.cost_details.as_ref())
                        && old_details.coverage != *coverage
                    {
                        prior.push(CostRecord {
                            cost: old_cost,
                            basis: old_basis,
                            coverage: old_details.coverage.clone(),
                            recorded_at: old_details.recorded_at,
                            is_partial: old_details.is_partial,
                        });
                    }
                    turn.cost = Some(*cost);
                    turn.cost_basis = Some(*basis);
                    turn.cost_details = Some(CostDetails {
                        coverage: coverage.clone(),
                        recorded_at: *recorded_at,
                        is_partial: *cost_is_partial,
                        prior,
                    });
                } else if let Some(details) = turn.cost_details.as_mut() {
                    details.is_partial = true;
                }
            }
            SessionChange::TurnOutputObserved {
                turn_id,
                observed_at,
            } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                turn.last_output_at = Some(
                    turn.last_output_at
                        .map_or(*observed_at, |current| current.max(*observed_at)),
                );
            }
            SessionChange::SubagentInterventionsChanged {
                subagent_interventions,
            } => {
                next.subagent_interventions
                    .clone_from(subagent_interventions);
            }
            SessionChange::SubagentUsageChanged { subagent_usage } => {
                // The reading arrives whole, derived by the one party that
                // can see across Sessions, so applying it is taking it as
                // given rather than adding anything up.
                next.subagent_usage = *subagent_usage;
            }
            SessionChange::TotalCostChanged { total_cost } => {
                next.total_cost = *total_cost;
            }
            SessionChange::SessionWorkingChanged { working_since } => {
                next.session.working_since = *working_since;
            }
            SessionChange::SessionMonitoringChanged { monitoring_since } => {
                next.session.monitoring_since = *monitoring_since;
            }
            SessionChange::SessionWatchesChanged { watches } => {
                next.watches.clone_from(watches);
            }
            SessionChange::SessionWaitingOnSubagentsChanged {
                waiting_on_subagents,
            } => {
                next.waiting_on_subagents = *waiting_on_subagents;
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
                if message.role != MessageRole::Agent && message.status != MessageStatus::Completed
                {
                    bail!("Only Agent Messages stream");
                }
                // A Delegation arrives whole, so it is added in its final
                // state, which may already be cut short by Suru's cap. Every
                // other Message is added before any content reaches it.
                if message.truncated && message.role.delegator().is_none() {
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
            SessionChange::DecisionAccepted { activity_id } => {
                let Some(Activity::Approval {
                    outcome, turn_id, ..
                }) = next.activities.iter_mut().find(|a| a.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if !outcome.is_answerable()
                    || !next.turns.iter().any(|turn| {
                        turn.id == *turn_id && turn.status == crate::protocol::TurnStatus::Active
                    })
                {
                    bail!("Approval is unavailable");
                }
                *outcome = crate::protocol::ApprovalOutcome::Submitting;
            }
            SessionChange::ApprovalSettled {
                activity_id,
                outcome,
                decision,
            } => {
                let Some(Activity::Approval {
                    outcome: current,
                    decision: stored,
                    ..
                }) = next.activities.iter_mut().find(|a| a.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if !current.is_live()
                    || (*outcome == crate::protocol::ApprovalOutcome::SubmissionRejected
                        && (*current != crate::protocol::ApprovalOutcome::Submitting
                            || decision.is_some()))
                    || matches!(
                        outcome,
                        crate::protocol::ApprovalOutcome::Pending
                            | crate::protocol::ApprovalOutcome::Submitting
                    )
                    || (*outcome == crate::protocol::ApprovalOutcome::Decided && decision.is_none())
                    || (*outcome != crate::protocol::ApprovalOutcome::Decided && decision.is_some())
                {
                    bail!("Approval is already unavailable");
                }
                *current = *outcome;
                *stored = *decision;
            }
            SessionChange::ApprovalFollowUpFailed { activity_id, error } => {
                let Some(Activity::Approval {
                    outcome,
                    follow_up_error,
                    ..
                }) = next.activities.iter_mut().find(|a| a.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if *outcome != crate::protocol::ApprovalOutcome::Decided
                    || follow_up_error.is_some()
                    || error.is_empty()
                {
                    bail!("Approval follow-up failure is invalid");
                }
                *follow_up_error = Some(error.clone());
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
                    Activity::ToolCall {
                        status,
                        output,
                        output_truncated,
                        omitted_parts,
                        ..
                    } if *status != ActivityStatus::Active
                        || !output.is_empty()
                        || *output_truncated
                        || *omitted_parts != 0
                ) {
                    bail!("Session update added a Tool Call Activity outside its initial state");
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
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
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
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!(
                        "Session update contained an invalid file-change Activity status transition"
                    );
                }
                *current_status = *status;
            }
            SessionChange::ToolCallInputChanged {
                activity_id,
                input,
                input_truncated,
            } => {
                let Some(Activity::ToolCall {
                    status,
                    input: current_input,
                    input_truncated: current_input_truncated,
                    ..
                }) = tool_call_activity(next, activity_id)?
                else {
                    bail!("Session update gave input to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update gave input to a terminal Tool Call Activity");
                }
                current_input.clone_from(input);
                *current_input_truncated = *input_truncated;
            }
            SessionChange::ToolCallOutputAppended {
                activity_id,
                content,
            } => {
                let Some(Activity::ToolCall {
                    status,
                    output,
                    output_truncated,
                    ..
                }) = tool_call_activity(next, activity_id)?
                else {
                    bail!("Session update appended Tool Call output to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended output to a terminal Tool Call Activity");
                }
                if *output_truncated {
                    bail!("Session update appended output past the cap that truncated a Tool Call");
                }
                output.push_str(content);
            }
            SessionChange::ToolCallOutputTruncated { activity_id } => {
                let Some(Activity::ToolCall {
                    status,
                    output_truncated,
                    ..
                }) = tool_call_activity(next, activity_id)?
                else {
                    bail!("Session update truncated the output of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the output of a terminal Tool Call Activity");
                }
                *output_truncated = true;
            }
            SessionChange::ToolCallStatusChanged {
                activity_id,
                status,
                omitted_parts,
            } => {
                let Some(Activity::ToolCall {
                    status: current_status,
                    omitted_parts: current_omitted_parts,
                    ..
                }) = tool_call_activity(next, activity_id)?
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!(
                        "Session update contained an invalid Tool Call Activity status transition"
                    );
                }
                *current_status = *status;
                *current_omitted_parts = *omitted_parts;
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
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
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
            SessionChange::SubagentModelChanged { activity_id, model } => {
                let Some(Activity::Subagent {
                    model: current_model,
                    ..
                }) = subagent_activity(next, activity_id)?
                else {
                    bail!("Session update identified a different Activity kind");
                };
                *current_model = Some(model.clone());
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
            SessionChange::WorkspaceChanged {
                workspace,
                checkout,
            } => {
                next.session.workspace = workspace.clone();
                next.session.checkout = checkout.clone();
            }
            SessionChange::SessionStatusChanged { status } => next.session.status = *status,
        }
    }
    next.revision = update.revision;
    let pending = next
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome,
                turn_id,
                ..
            } if outcome.is_answerable()
                && next
                    .turns
                    .iter()
                    .any(|turn| turn.id == *turn_id && turn.status == TurnStatus::Active) =>
            {
                Some(approval.id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let submitting = next
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: crate::protocol::ApprovalOutcome::Submitting,
                ..
            } => Some(approval.id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if next.pending_approvals != pending || next.submitting_approvals != submitting {
        next.pending_approvals = pending;
        next.submitting_approvals = submitting;
        next.pending_approvals_revision = update.revision;
    }
    Ok(())
}

/// Records descriptors among those a Session already carries, keeping one per
/// Attachment in id order. An id names the same bytes wherever it is bound, so
/// a descriptor arriving again only takes the place of the one it repeats.
pub(crate) fn describe_attachments(
    known: &mut Vec<AttachmentDescriptor>,
    described: impl IntoIterator<Item = AttachmentDescriptor>,
) {
    for descriptor in described {
        match known.binary_search_by(|existing| existing.id.cmp(&descriptor.id)) {
            Ok(index) => known[index] = descriptor,
            Err(index) => known.insert(index, descriptor),
        }
    }
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

/// Resolves the Activity a Tool Call change names, on the same terms as
/// [`reasoning_activity`].
fn tool_call_activity<'a>(
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
    Ok(matches!(activity, Activity::ToolCall { .. }).then_some(activity))
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

    fn image(id: &str, width: u32) -> AttachmentDescriptor {
        AttachmentDescriptor {
            id: crate::protocol::AttachmentId::new(id),
            kind: crate::protocol::AttachmentKind::Image { width, height: 1 },
            mime_type: "image/png".to_owned(),
            byte_length: 64,
        }
    }

    #[test]
    fn described_attachments_are_kept_once_each_in_id_order() {
        let mut known = vec![image("b", 2)];
        describe_attachments(&mut known, [image("c", 3), image("a", 1), image("b", 2)]);
        assert_eq!(known, vec![image("a", 1), image("b", 2), image("c", 3)]);

        describe_attachments(&mut known, [image("a", 1)]);
        assert_eq!(
            known,
            vec![image("a", 1), image("b", 2), image("c", 3)],
            "a descriptor described again changes nothing"
        );
    }

    #[test]
    fn a_client_reads_turn_timing_and_usage_off_the_changes_the_server_committed() {
        let session_id = SessionId::new();
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let mut snapshot = SessionSnapshot {
            title: String::new(),
            icon: None,
            session: Session {
                checkout: None,
                context_fill: None,
                id: session_id,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: PathBuf::from("/workspace"),
                },
                workspace: Workspace::directory(PathBuf::from("/workspace")),
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                approval_posture: None,
                status: SessionStatus::Idle,
                working_since: None,
                monitoring_since: None,
                parent: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: vec![Prompt {
                id: prompt_id,
                text: "Work on this".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
                delivery: PromptDelivery::Steer,
                admission_order: PromptOrder::INITIAL,
                status: PromptStatus::Pending,
            }],
            turns: Vec::new(),
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            waiting_on_subagents: None,
            subagent_usage: None,
            total_cost: None,
            attachments: Vec::new(),
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
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
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
                    cost_coverage: Some(crate::protocol::CostCoverage::Turn),
                    cost_is_partial: false,
                    cost_recorded_at: Some(SessionTimestamp(2)),
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
