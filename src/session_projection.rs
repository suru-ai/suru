//! Shared projection rules for authoritative and client-side Session state.

use anyhow::{Result, bail};

use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AttachmentDescriptor, CompactionTrigger, CostDetails,
    CostRecord, MessageRole, MessageStatus, PromptDelivery, PromptStatus, SessionChange,
    SessionSnapshot, SessionUpdate, TranscriptItem, TurnStatus,
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
            SessionChange::PromptWithdrawn {
                prompt_id,
                withdrawal,
            } => {
                let Some(prompt) = next
                    .prompts
                    .iter_mut()
                    .find(|prompt| prompt.id == *prompt_id)
                else {
                    bail!("Session update referenced an unknown Prompt");
                };
                if prompt.status != PromptStatus::Pending {
                    bail!("Session update withdrew a Prompt that was not Pending");
                }
                prompt.status = PromptStatus::Cancelled;
                prompt.withdrawal = Some(*withdrawal);
            }
            SessionChange::ContextFillChanged { context_fill } => {
                next.session.context_fill = *context_fill;
            }
            SessionChange::TurnAdded { turn } => {
                if !turn.has_valid_cost_attribution() {
                    bail!("Session update added a Turn with a Cost lacking exactly one Cost Basis");
                }
                if !turn.has_valid_opening() {
                    bail!(
                        "Session update added a Turn begun by both a Prompt and a Compaction request"
                    );
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
            SessionChange::TotalCostChanged {
                total_cost,
                own_cost,
            } => {
                next.total_cost = *total_cost;
                next.own_cost = *own_cost;
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
                let Some(turn) = next.turns.iter().find(|turn| turn.id == activity.turn_id())
                else {
                    bail!("Session update referenced an unknown Turn");
                };
                // Whether a Compaction was manual is read from the Turn that
                // holds it (ADR 0041), so the two never disagree.
                if let Activity::Compaction { trigger, .. } = activity
                    && (*trigger == CompactionTrigger::Manual) != turn.compaction_requested
                {
                    bail!("Session update added a Compaction its Turn says the other kind of");
                }
                // Only the user's request asks anything of a summary.
                if let Activity::Compaction {
                    trigger: CompactionTrigger::Automatic,
                    instructions: Some(_),
                    ..
                } = activity
                {
                    bail!("Session update added an automatic Compaction carrying instructions");
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
                // An Active Compaction may know the Context Fill it began
                // from, but nothing of how it ends until it Settles. One the
                // Provider reported only ending is added already settled.
                if matches!(
                    activity,
                    Activity::Compaction {
                        status: ActivityStatus::Active,
                        after_tokens,
                        error,
                        ..
                    } if after_tokens.is_some() || error.is_some()
                ) {
                    bail!("Session update added an Active Compaction that had already ended");
                }
                // A stopped Compaction left the context as it was, so it has
                // no change in Context Fill to carry.
                if matches!(
                    activity,
                    Activity::Compaction {
                        status: ActivityStatus::Interrupted,
                        before_tokens,
                        after_tokens,
                        ..
                    } if before_tokens.is_some() || after_tokens.is_some()
                ) {
                    bail!("Session update added a stopped Compaction with a Context Fill");
                }
                if let Activity::Compaction {
                    status,
                    summary,
                    summary_truncated,
                    ..
                } = activity
                    && !compaction_summary_fits(*status, summary.as_ref(), *summary_truncated)
                {
                    bail!("Session update added a Compaction with a summary it cannot have");
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
            SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::Compaction {
                    status: current_status,
                    before_tokens: current_before_tokens,
                    after_tokens: current_after_tokens,
                    error: current_error,
                    summary: current_summary,
                    summary_truncated: current_summary_truncated,
                    ..
                } = activity
                else {
                    bail!("Session update settled a different Activity kind as a Compaction");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!("Session update contained an invalid Compaction status transition");
                }
                if *status == ActivityStatus::Interrupted
                    && (before_tokens.is_some() || after_tokens.is_some())
                {
                    bail!("Session update settled a Compaction as stopped with a Context Fill");
                }
                if !compaction_summary_fits(*status, summary.as_ref(), *summary_truncated) {
                    bail!("Session update settled a Compaction with a summary it cannot have");
                }
                *current_status = *status;
                *current_before_tokens = *before_tokens;
                *current_after_tokens = *after_tokens;
                current_error.clone_from(error);
                current_summary.clone_from(summary);
                *current_summary_truncated = *summary_truncated;
            }
            SessionChange::CompactionAfterMeasured {
                activity_id,
                after_tokens,
            } => {
                let Some(activity) = next
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Session update referenced an unknown Activity");
                };
                let Activity::Compaction {
                    status,
                    after_tokens: current_after_tokens,
                    ..
                } = activity
                else {
                    bail!("Session update measured a different Activity kind as a Compaction");
                };
                // Only a completed Compaction freed room a reading could
                // measure, and a count already known — the Provider's own
                // above all — is never replaced by one.
                if *status != ActivityStatus::Completed || current_after_tokens.is_some() {
                    bail!("Session update measured a Compaction that takes no reading after it");
                }
                *current_after_tokens = Some(*after_tokens);
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

/// Whether a Compaction standing at `status` can carry the summary it names.
/// Only a completed Compaction left the Agent a summary, and a cap can only
/// have cut a summary that was stored.
fn compaction_summary_fits(
    status: ActivityStatus,
    summary: Option<&String>,
    summary_truncated: bool,
) -> bool {
    match summary {
        Some(_) => status == ActivityStatus::Completed,
        None => !summary_truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::protocol::{
        Cost, CostBasis, ModelAvailability, Prompt, PromptId, PromptOrder, PromptWithdrawal,
        Session, SessionId, SessionRevision, SessionStatus, SessionTimestamp, Turn, TurnId, Usage,
        Workspace,
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

    /// A Session with nothing in it yet, for a test to build the state it is
    /// about on top of.
    fn empty_snapshot(session_id: SessionId) -> SessionSnapshot {
        SessionSnapshot {
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
            prompts: Vec::new(),
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
            own_cost: None,
            attachments: Vec::new(),
        }
    }

    /// A Turn at work that no Prompt began.
    fn active_continuation(turn_id: TurnId) -> Turn {
        Turn {
            id: turn_id,
            prompt_id: None,
            compaction_requested: false,
            agent: None,
            status: TurnStatus::Active,
            started_at: Some(SessionTimestamp(1_755_000_000_000)),
            settled_at: None,
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }
    }

    #[test]
    fn a_prompt_the_session_withdraws_is_cancelled_saying_why_and_only_once() {
        let session_id = SessionId::new();
        let prompt_id = PromptId::new();
        let withdrawal = PromptWithdrawal::CompactionUnfinished {
            turn_id: TurnId::new(),
        };
        let mut snapshot = empty_snapshot(session_id);
        snapshot.prompts.push(Prompt {
            id: prompt_id,
            text: "Now the lexer".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
            withdrawal: None,
            author: None,
        });
        let withdrawn = |revision| SessionUpdate {
            session_id,
            revision: SessionRevision(revision),
            changes: vec![SessionChange::PromptWithdrawn {
                prompt_id,
                withdrawal,
            }],
        };

        apply_update(&mut snapshot, &withdrawn(SessionRevision::INITIAL.0 + 1))
            .expect("a Pending Prompt may be withdrawn");
        assert_eq!(snapshot.prompts[0].status, PromptStatus::Cancelled);
        assert_eq!(snapshot.prompts[0].withdrawal, Some(withdrawal));
        assert!(
            apply_update(&mut snapshot, &withdrawn(SessionRevision::INITIAL.0 + 2)).is_err(),
            "only a Pending Prompt is withdrawn"
        );
    }

    #[test]
    fn a_client_reads_turn_timing_and_usage_off_the_changes_the_server_committed() {
        let session_id = SessionId::new();
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.prompts.push(Prompt {
            id: prompt_id,
            text: "Work on this".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
            withdrawal: None,
            author: None,
        });

        apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes: vec![SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id: Some(prompt_id),
                        compaction_requested: false,
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

    fn compaction(id: ActivityId, turn_id: TurnId) -> Activity {
        Activity::Compaction {
            id,
            turn_id,
            status: ActivityStatus::Active,
            trigger: crate::protocol::CompactionTrigger::Automatic,
            instructions: None,
            before_tokens: None,
            after_tokens: None,
            error: None,
            summary: None,
            summary_truncated: false,
        }
    }

    #[test]
    fn a_compaction_settles_once_from_active_with_what_the_provider_reported() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let activity_id = ActivityId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.turns.push(active_continuation(turn_id));
        let initial = snapshot.revision.0;
        let update = |step, changes| SessionUpdate {
            session_id,
            revision: SessionRevision(initial + step),
            changes,
        };

        apply_update(
            &mut snapshot,
            &update(
                1,
                vec![SessionChange::ActivityAdded {
                    activity: compaction(activity_id, turn_id),
                }],
            ),
        )
        .expect("a Compaction joins its Turn Active");
        assert_eq!(
            snapshot.transcript,
            vec![TranscriptItem::Activity { activity_id }],
            "the Compaction takes its place in the Transcript"
        );

        apply_update(
            &mut snapshot,
            &update(
                2,
                vec![SessionChange::CompactionSettled {
                    activity_id,
                    status: ActivityStatus::Completed,
                    before_tokens: Some(182_000),
                    after_tokens: Some(31_000),
                    error: None,
                    summary: None,
                    summary_truncated: false,
                }],
            ),
        )
        .expect("the Compaction settles");
        assert_eq!(
            snapshot.activities,
            vec![Activity::Compaction {
                id: activity_id,
                turn_id,
                status: ActivityStatus::Completed,
                trigger: crate::protocol::CompactionTrigger::Automatic,
                instructions: None,
                before_tokens: Some(182_000),
                after_tokens: Some(31_000),
                error: None,
                summary: None,
                summary_truncated: false,
            }]
        );

        let mut settled_again = snapshot.clone();
        assert!(
            apply_update(
                &mut settled_again,
                &update(
                    3,
                    vec![SessionChange::CompactionSettled {
                        activity_id,
                        status: ActivityStatus::Failed,
                        before_tokens: None,
                        after_tokens: None,
                        error: Some("too late".to_owned()),
                        summary: None,
                        summary_truncated: false,
                    }],
                ),
            )
            .is_err(),
            "a settled Compaction accepts no second settle"
        );
        let mut never_settled = snapshot.clone();
        let second = ActivityId::new();
        never_settled.activities.push(compaction(second, turn_id));
        assert!(
            apply_update(
                &mut never_settled,
                &update(
                    3,
                    vec![SessionChange::CompactionSettled {
                        activity_id: second,
                        status: ActivityStatus::Active,
                        before_tokens: None,
                        after_tokens: None,
                        error: None,
                        summary: None,
                        summary_truncated: false,
                    }],
                ),
            )
            .is_err(),
            "settling names a terminal status"
        );
    }

    #[test]
    fn an_active_compaction_is_added_knowing_nothing_of_how_it_ends() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.turns.push(active_continuation(turn_id));
        let added =
            |status, before_tokens, after_tokens, error: Option<&str>| Activity::Compaction {
                id: ActivityId::new(),
                turn_id,
                status,
                trigger: crate::protocol::CompactionTrigger::Automatic,
                instructions: None,
                before_tokens,
                after_tokens,
                error: error.map(ToOwned::to_owned),
                summary: None,
                summary_truncated: false,
            };
        for (activity, what) in [
            (
                added(ActivityStatus::Active, None, Some(31_000), None),
                "already measured after it settled",
            ),
            (
                added(ActivityStatus::Active, None, None, Some("failed")),
                "already failed",
            ),
        ] {
            assert!(
                apply_update(
                    &mut snapshot.clone(),
                    &SessionUpdate {
                        session_id,
                        revision: SessionRevision(snapshot.revision.0 + 1),
                        changes: vec![SessionChange::ActivityAdded { activity }],
                    },
                )
                .is_err(),
                "an Active Compaction {what} is refused"
            );
        }
        for (activity, what) in [
            (
                added(ActivityStatus::Active, Some(182_000), None, None),
                "an Active one that knows only the Context Fill it began from",
            ),
            (
                added(ActivityStatus::Completed, Some(182_000), Some(31_000), None),
                "one the Provider reported only completing",
            ),
            (
                added(ActivityStatus::Failed, None, None, Some("failed")),
                "one the Provider reported only failing",
            ),
        ] {
            apply_update(
                &mut snapshot.clone(),
                &SessionUpdate {
                    session_id,
                    revision: SessionRevision(snapshot.revision.0 + 1),
                    changes: vec![SessionChange::ActivityAdded { activity }],
                },
            )
            .unwrap_or_else(|error| panic!("{what} is added: {error}"));
        }
    }

    #[test]
    fn a_stopped_compaction_carries_no_context_fill() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let activity_id = ActivityId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.turns.push(active_continuation(turn_id));
        let with = |id, status, before_tokens, after_tokens| Activity::Compaction {
            id,
            turn_id,
            status,
            trigger: crate::protocol::CompactionTrigger::Automatic,
            instructions: None,
            before_tokens,
            after_tokens,
            error: None,
            summary: None,
            summary_truncated: false,
        };
        snapshot.activities.push(with(
            activity_id,
            ActivityStatus::Active,
            Some(182_000),
            None,
        ));
        let applied = |snapshot: &SessionSnapshot, change| {
            apply_update(
                &mut snapshot.clone(),
                &SessionUpdate {
                    session_id,
                    revision: SessionRevision(snapshot.revision.0 + 1),
                    changes: vec![change],
                },
            )
        };
        let stopped = |before_tokens, after_tokens| SessionChange::CompactionSettled {
            activity_id,
            status: ActivityStatus::Interrupted,
            before_tokens,
            after_tokens,
            error: None,
            summary: None,
            summary_truncated: false,
        };
        let added = |before_tokens, after_tokens| SessionChange::ActivityAdded {
            activity: with(
                ActivityId::new(),
                ActivityStatus::Interrupted,
                before_tokens,
                after_tokens,
            ),
        };

        for (change, what) in [
            (stopped(Some(182_000), None), "settled keeping its before"),
            (stopped(None, Some(31_000)), "settled with an after"),
            (added(Some(182_000), None), "added with a before"),
            (added(None, Some(31_000)), "added with an after"),
        ] {
            assert!(
                applied(&snapshot, change).is_err(),
                "a stopped Compaction {what} is refused"
            );
        }
        for (change, what) in [
            (stopped(None, None), "settled"),
            (added(None, None), "added"),
        ] {
            applied(&snapshot, change)
                .unwrap_or_else(|error| panic!("a stopped Compaction {what} bare: {error}"));
        }
    }

    #[test]
    fn a_compaction_keeps_the_summary_it_left_and_only_a_completed_one_leaves_one() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.turns.push(active_continuation(turn_id));
        let activity_id = ActivityId::new();
        snapshot.activities.push(compaction(activity_id, turn_id));
        let update = |changes| SessionUpdate {
            session_id,
            revision: SessionRevision(snapshot.revision.0 + 1),
            changes,
        };
        let settled =
            |status, summary: Option<&str>, summary_truncated| SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens: None,
                after_tokens: None,
                error: None,
                summary: summary.map(ToOwned::to_owned),
                summary_truncated,
            };

        let mut summarised = snapshot.clone();
        apply_update(
            &mut summarised,
            &update(vec![settled(
                ActivityStatus::Completed,
                Some("The parser work is half done."),
                true,
            )]),
        )
        .expect("a completed Compaction settles with its summary");
        let [
            Activity::Compaction {
                summary,
                summary_truncated,
                ..
            },
        ] = &summarised.activities[..]
        else {
            panic!("one Compaction stands: {:?}", summarised.activities);
        };
        assert_eq!(
            (summary.as_deref(), *summary_truncated),
            (Some("The parser work is half done."), true),
            "the summary is kept beside whether the cap cut it"
        );

        for (change, what) in [
            (
                settled(ActivityStatus::Failed, Some("half a summary"), false),
                "a failed Compaction left no summary",
            ),
            (
                settled(ActivityStatus::Interrupted, Some("half a summary"), false),
                "an interrupted Compaction left no summary",
            ),
            (
                settled(ActivityStatus::Completed, None, true),
                "a cap cut nothing that was never stored",
            ),
        ] {
            assert!(
                apply_update(&mut snapshot.clone(), &update(vec![change])).is_err(),
                "{what}"
            );
        }
        let added = |status, summary: Option<&str>| SessionChange::ActivityAdded {
            activity: Activity::Compaction {
                id: ActivityId::new(),
                turn_id,
                status,
                trigger: crate::protocol::CompactionTrigger::Automatic,
                instructions: None,
                before_tokens: None,
                after_tokens: None,
                error: None,
                summary: summary.map(ToOwned::to_owned),
                summary_truncated: false,
            },
        };
        assert!(
            apply_update(
                &mut snapshot.clone(),
                &update(vec![added(ActivityStatus::Active, Some("too soon"))])
            )
            .is_err(),
            "an Active Compaction has left no summary yet"
        );
        apply_update(
            &mut snapshot.clone(),
            &update(vec![added(ActivityStatus::Completed, Some("all done"))]),
        )
        .expect("one the Provider reported only completing is added with its summary");
    }

    #[test]
    fn a_completed_compaction_takes_one_after_reading_and_never_over_a_known_count() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut snapshot = empty_snapshot(session_id);
        snapshot.turns.push(active_continuation(turn_id));
        let settled = |status, after_tokens| Activity::Compaction {
            id: ActivityId::new(),
            turn_id,
            status,
            trigger: crate::protocol::CompactionTrigger::Automatic,
            instructions: None,
            before_tokens: Some(182_000),
            after_tokens,
            error: None,
            summary: None,
            summary_truncated: false,
        };
        let measured = |snapshot: &mut SessionSnapshot, activity_id| {
            let revision = SessionRevision(snapshot.revision.0 + 1);
            apply_update(
                snapshot,
                &SessionUpdate {
                    session_id,
                    revision,
                    changes: vec![SessionChange::CompactionAfterMeasured {
                        activity_id,
                        after_tokens: 35_000,
                    }],
                },
            )
        };

        let unmeasured = settled(ActivityStatus::Completed, None);
        let mut measuring = snapshot.clone();
        measuring.activities.push(unmeasured.clone());
        measured(&mut measuring, unmeasured.id())
            .expect("a completed Compaction with no after takes the reading");
        assert_eq!(
            measuring.activities,
            vec![Activity::Compaction {
                id: unmeasured.id(),
                turn_id,
                status: ActivityStatus::Completed,
                trigger: crate::protocol::CompactionTrigger::Automatic,
                instructions: None,
                before_tokens: Some(182_000),
                after_tokens: Some(35_000),
                error: None,
                summary: None,
                summary_truncated: false,
            }],
            "only its after changes"
        );

        for (activity, what) in [
            (
                measuring.activities[0].clone(),
                "a Compaction already measured after it",
            ),
            (
                settled(ActivityStatus::Completed, Some(31_000)),
                "a Compaction whose Provider reported its after",
            ),
            (
                settled(ActivityStatus::Active, None),
                "a Compaction still summarising",
            ),
            (
                settled(ActivityStatus::Failed, None),
                "a Compaction that freed nothing",
            ),
            (
                settled(ActivityStatus::Interrupted, None),
                "a Compaction that was stopped",
            ),
        ] {
            let mut refusing = snapshot.clone();
            let activity_id = activity.id();
            refusing.activities.push(activity);
            assert!(
                measured(&mut refusing, activity_id).is_err(),
                "{what} takes no reading after it"
            );
        }
        let mut other_kind = snapshot.clone();
        let error = Activity::Error {
            id: ActivityId::new(),
            turn_id,
            text: "Not a Compaction".to_owned(),
        };
        let error_id = error.id();
        other_kind.activities.push(error);
        assert!(
            measured(&mut other_kind, error_id).is_err(),
            "only a Compaction is measured as one"
        );
    }

    #[test]
    fn a_manual_compaction_stands_only_in_a_turn_a_compaction_request_began() {
        let session_id = SessionId::new();
        let continuation = active_continuation(TurnId::new());
        let requested = Turn {
            compaction_requested: true,
            ..active_continuation(TurnId::new())
        };
        let compaction = |turn: &Turn, trigger| Activity::Compaction {
            id: ActivityId::new(),
            turn_id: turn.id,
            status: ActivityStatus::Active,
            trigger,
            instructions: None,
            before_tokens: None,
            after_tokens: None,
            error: None,
            summary: None,
            summary_truncated: false,
        };
        let adding = |turn: &Turn, activity: Activity| {
            let mut snapshot = empty_snapshot(session_id);
            snapshot.turns.push(turn.clone());
            let revision = SessionRevision(snapshot.revision.0 + 1);
            apply_update(
                &mut snapshot,
                &SessionUpdate {
                    session_id,
                    revision,
                    changes: vec![SessionChange::ActivityAdded { activity }],
                },
            )
        };

        adding(
            &requested,
            compaction(&requested, CompactionTrigger::Manual),
        )
        .expect("the Turn a request began holds a manual Compaction");
        adding(
            &continuation,
            compaction(&continuation, CompactionTrigger::Automatic),
        )
        .expect("any other Turn holds an automatic one");
        assert!(
            adding(
                &requested,
                compaction(&requested, CompactionTrigger::Automatic)
            )
            .is_err(),
            "the Turn a request began holds no automatic Compaction"
        );
        assert!(
            adding(
                &continuation,
                compaction(&continuation, CompactionTrigger::Manual)
            )
            .is_err(),
            "no Turn a request did not begin holds a manual Compaction"
        );
        let asked = |activity: Activity| match activity {
            Activity::Compaction {
                id,
                turn_id,
                status,
                trigger,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
                ..
            } => Activity::Compaction {
                id,
                turn_id,
                status,
                trigger,
                instructions: Some("Keep the parser notes".to_owned()),
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            },
            _ => unreachable!(),
        };
        adding(
            &requested,
            asked(compaction(&requested, CompactionTrigger::Manual)),
        )
        .expect("a manual Compaction carries the instructions it was asked with");
        assert!(
            adding(
                &continuation,
                asked(compaction(&continuation, CompactionTrigger::Automatic))
            )
            .is_err(),
            "no one asked anything of an automatic Compaction"
        );

        let mut snapshot = empty_snapshot(session_id);
        snapshot.prompts.push(Prompt {
            id: PromptId::new(),
            text: "Compact this".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Queue,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
            withdrawal: None,
            author: None,
        });
        let both = Turn {
            prompt_id: Some(snapshot.prompts[0].id),
            ..requested.clone()
        };
        assert!(
            apply_update(
                &mut snapshot.clone(),
                &SessionUpdate {
                    session_id,
                    revision: SessionRevision(snapshot.revision.0 + 1),
                    changes: vec![SessionChange::TurnAdded { turn: both }],
                },
            )
            .is_err(),
            "no Turn is begun by both a Prompt and a Compaction request"
        );
    }
}
