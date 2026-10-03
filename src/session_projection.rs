//! Shared projection rules for authoritative and client-side Session state.
//!
//! A batch of changes lands in two phases. [`validate_changes`] reads the
//! snapshot and refuses the batch while nothing has moved, resolving where in
//! the snapshot each change lands; [`ValidatedChanges::apply`] then applies it
//! in place, unable to fail. A Session that holds every Message and Activity
//! it has ever streamed is therefore never copied to keep a refused batch from
//! leaving half of itself behind.
//!
//! [`agent_reading`] projects the same state as text for an Agent to read.

pub(crate) mod agent_reading;

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentIdentity, ApprovalOutcome, AttachmentDescriptor,
    CompactionTrigger, CostDetails, CostRecord, Message, MessageRole, MessageStatus, Prompt,
    PromptDelivery, PromptStatus, QuestionnaireOutcome, SessionChange, SessionRevision,
    SessionSnapshot, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus,
};

/// Applies `update` to `snapshot` in place, or refuses it and leaves the
/// snapshot exactly as it was.
pub(crate) fn apply_update(snapshot: &mut SessionSnapshot, update: &SessionUpdate) -> Result<()> {
    if snapshot.session.id != update.session_id {
        bail!("Session update targeted a different Session");
    }
    if !update.revision.immediately_follows(snapshot.revision) {
        bail!("Session update revision is not monotonic");
    }
    validate_changes(snapshot, &update.changes)?.apply(snapshot, &update.changes);
    close_revision(snapshot, update.revision);
    Ok(())
}

/// Checks that `changes`, in order, apply cleanly to `snapshot`, reading it
/// without moving anything. Each change is checked against the Session as the
/// changes before it in the batch leave it, so a batch may add a Turn and
/// stream into it, or settle a Prompt it promoted, as one revision.
pub(crate) fn validate_changes(
    snapshot: &SessionSnapshot,
    changes: &[SessionChange],
) -> Result<ValidatedChanges> {
    let mut validation = Validation::new(snapshot);
    let targets = changes
        .iter()
        .map(|change| validation.validate(change))
        .collect::<Result<Vec<_>>>()?;
    Ok(ValidatedChanges { targets })
}

/// A batch [`validate_changes`] found applies cleanly to the snapshot it read,
/// carrying where in that snapshot each change lands — the Prompt, Turn,
/// Message, or Activity it names, by position — so applying it neither looks
/// anything up again nor fails.
#[must_use = "a validated batch has moved nothing until it is applied"]
#[derive(Debug)]
pub(crate) struct ValidatedChanges {
    targets: Vec<Option<usize>>,
}

/// Why applying a validated change cannot miss what it names.
const RESOLVED: &str = "validation resolved the change against this snapshot";

impl ValidatedChanges {
    /// Applies `changes` to `snapshot` in place. They must be the changes that
    /// were validated, and the snapshot the one they were validated against,
    /// unmoved since.
    pub(crate) fn apply(self, snapshot: &mut SessionSnapshot, changes: &[SessionChange]) {
        debug_assert_eq!(self.targets.len(), changes.len(), "{RESOLVED}");
        let next = snapshot;
        for (change, target) in changes.iter().zip(self.targets) {
            let resolved = || target.expect(RESOLVED);
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
                SessionChange::PromptAdded { prompt } => next.prompts.push(prompt.clone()),
                SessionChange::PromptDeliveryChanged { delivery, .. } => {
                    next.prompts[resolved()].delivery = *delivery;
                }
                SessionChange::PromptStatusChanged { status, .. } => {
                    next.prompts[resolved()].status = *status;
                }
                SessionChange::PromptWithdrawn { withdrawal, .. } => {
                    let prompt = &mut next.prompts[resolved()];
                    prompt.status = PromptStatus::Cancelled;
                    prompt.withdrawal = Some(*withdrawal);
                }
                SessionChange::PromptTaken { taking, .. } => {
                    next.prompts[resolved()].taken = Some(*taking);
                }
                SessionChange::ContextFillChanged { context_fill } => {
                    next.session.context_fill = *context_fill;
                }
                SessionChange::TurnAdded { turn } => {
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
                SessionChange::TurnAgentChanged { agent, .. }
                | SessionChange::SubagentAgentChanged { agent, .. } => {
                    let turn = &mut next.turns[resolved()];
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
                    usage,
                    cost,
                    cost_basis,
                    cost_coverage,
                    cost_is_partial,
                    cost_recorded_at,
                    ..
                } => {
                    let turn = &mut next.turns[resolved()];
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
                SessionChange::TurnOutputObserved { observed_at, .. } => {
                    let turn = &mut next.turns[resolved()];
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
                    next.messages.push(message.clone());
                    next.transcript.push(TranscriptItem::Message {
                        message_id: message.id,
                    });
                }
                SessionChange::MessageContentAppended { content, .. } => {
                    next.messages[resolved()].content.push_str(content);
                }
                SessionChange::MessageTruncated { .. } => {
                    next.messages[resolved()].truncated = true;
                }
                SessionChange::MessageCompleted { .. } => {
                    next.messages[resolved()].status = MessageStatus::Completed;
                }
                SessionChange::DecisionAccepted { .. } => {
                    let Activity::Approval { outcome, .. } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *outcome = ApprovalOutcome::Submitting;
                }
                SessionChange::ApprovalSettled {
                    outcome, decision, ..
                } => {
                    let Activity::Approval {
                        outcome: current,
                        decision: stored,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current = *outcome;
                    *stored = *decision;
                }
                SessionChange::ApprovalFollowUpFailed { error, .. } => {
                    let Activity::Approval {
                        follow_up_error, ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *follow_up_error = Some(error.clone());
                }
                SessionChange::QuestionnaireAccepted { .. } => {
                    let Activity::Questionnaire { outcome, .. } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *outcome = QuestionnaireOutcome::Submitting;
                }
                SessionChange::QuestionnaireSettled {
                    outcome,
                    answer,
                    author,
                    settled_at,
                    ..
                } => {
                    let Activity::Questionnaire {
                        outcome: current,
                        answer: stored,
                        author: answered_by,
                        settled_at: settled,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current = *outcome;
                    *stored = answer.clone();
                    answered_by.clone_from(author);
                    *settled = *settled_at;
                }
                SessionChange::ActivityAdded { activity } => {
                    next.activities.push(activity.clone());
                    next.transcript.push(TranscriptItem::Activity {
                        activity_id: activity.id(),
                    });
                }
                SessionChange::CommandOutputAppended { content, .. } => {
                    let Activity::Command { output, .. } = &mut next.activities[resolved()] else {
                        unreachable!("{RESOLVED}");
                    };
                    output.push_str(content);
                }
                SessionChange::CommandOutputTruncated { .. } => {
                    let Activity::Command {
                        output_truncated, ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *output_truncated = true;
                }
                SessionChange::CommandStatusChanged {
                    status,
                    exit_status,
                    ..
                } => {
                    let Activity::Command {
                        status: current_status,
                        exit_status: current_exit_status,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                    *current_exit_status = *exit_status;
                }
                SessionChange::FileChangeUpdated { changes, .. } => {
                    let Activity::FileChange {
                        changes: current_changes,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    current_changes.clone_from(changes);
                }
                SessionChange::FileChangeStatusChanged { status, .. } => {
                    let Activity::FileChange {
                        status: current_status,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                }
                SessionChange::ToolCallInputChanged {
                    input,
                    input_truncated,
                    ..
                } => {
                    let Activity::ToolCall {
                        input: current_input,
                        input_truncated: current_input_truncated,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    current_input.clone_from(input);
                    *current_input_truncated = *input_truncated;
                }
                SessionChange::ToolCallOutputAppended { content, .. } => {
                    let Activity::ToolCall { output, .. } = &mut next.activities[resolved()] else {
                        unreachable!("{RESOLVED}");
                    };
                    output.push_str(content);
                }
                SessionChange::ToolCallOutputTruncated { .. } => {
                    let Activity::ToolCall {
                        output_truncated, ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *output_truncated = true;
                }
                SessionChange::ToolCallStatusChanged {
                    status,
                    omitted_parts,
                    ..
                } => {
                    let Activity::ToolCall {
                        status: current_status,
                        omitted_parts: current_omitted_parts,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                    *current_omitted_parts = *omitted_parts;
                }
                SessionChange::ReasoningTitleChanged { title, .. } => {
                    let Activity::Reasoning {
                        title: current_title,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_title = Some(title.clone());
                }
                SessionChange::ReasoningContentAppended { content, .. } => {
                    let Activity::Reasoning {
                        content: current_content,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    current_content.push_str(content);
                }
                SessionChange::ReasoningContentTruncated { .. } => {
                    let Activity::Reasoning {
                        content_truncated, ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *content_truncated = true;
                }
                SessionChange::ReasoningStatusChanged {
                    status,
                    duration_ms,
                    ..
                } => {
                    let Activity::Reasoning {
                        status: current_status,
                        duration_ms: current_duration_ms,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                    *current_duration_ms = *duration_ms;
                }
                SessionChange::SubagentDescriptionChanged { description, .. } => {
                    let Activity::Subagent {
                        description: current_description,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    current_description.clone_from(description);
                }
                SessionChange::SubagentModelChanged { model, .. } => {
                    let Activity::Subagent {
                        model: current_model,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_model = Some(model.clone());
                }
                SessionChange::SubagentStatusChanged {
                    status,
                    duration_ms,
                    ..
                } => {
                    let Activity::Subagent {
                        status: current_status,
                        duration_ms: current_duration_ms,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                    *current_duration_ms = *duration_ms;
                }
                SessionChange::CompactionSettled {
                    status,
                    before_tokens,
                    after_tokens,
                    error,
                    summary,
                    summary_truncated,
                    ..
                } => {
                    let Activity::Compaction {
                        status: current_status,
                        before_tokens: current_before_tokens,
                        after_tokens: current_after_tokens,
                        error: current_error,
                        summary: current_summary,
                        summary_truncated: current_summary_truncated,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_status = *status;
                    *current_before_tokens = *before_tokens;
                    *current_after_tokens = *after_tokens;
                    current_error.clone_from(error);
                    current_summary.clone_from(summary);
                    *current_summary_truncated = *summary_truncated;
                }
                SessionChange::CompactionAfterMeasured { after_tokens, .. } => {
                    let Activity::Compaction {
                        after_tokens: current_after_tokens,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    *current_after_tokens = Some(*after_tokens);
                }
                SessionChange::SubsessionTitleChanged { title, .. } => {
                    let Activity::Subsession {
                        title: current_title,
                        ..
                    } = &mut next.activities[resolved()]
                    else {
                        unreachable!("{RESOLVED}");
                    };
                    current_title.clone_from(title);
                }
                SessionChange::TurnStatusChanged {
                    status, settled_at, ..
                } => {
                    let turn = &mut next.turns[resolved()];
                    turn.status = *status;
                    turn.settled_at = *settled_at;
                }
                SessionChange::WorkspaceChanged {
                    workspace,
                    checkout,
                } => {
                    next.session.workspace.clone_from(workspace);
                    next.session.checkout.clone_from(checkout);
                }
                SessionChange::SessionStatusChanged { status } => next.session.status = *status,
            }
        }
    }
}

/// Moves `snapshot` to `revision` once every change the revision carries has
/// been applied, and re-derives the Approvals standing open in it. Those are
/// read off the whole Session rather than any one change, so they are taken
/// once the batch is in rather than change by change.
pub(crate) fn close_revision(snapshot: &mut SessionSnapshot, revision: SessionRevision) {
    snapshot.revision = revision;
    let pending = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome,
                turn_id,
                ..
            } if outcome.is_answerable()
                && snapshot
                    .turns
                    .iter()
                    .any(|turn| turn.id == *turn_id && turn.status == TurnStatus::Active) =>
            {
                Some(approval.id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let submitting = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: ApprovalOutcome::Submitting,
                ..
            } => Some(approval.id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if snapshot.pending_approvals != pending || snapshot.submitting_approvals != submitting {
        snapshot.pending_approvals = pending;
        snapshot.submitting_approvals = submitting;
        snapshot.pending_approvals_revision = revision;
    }
}

/// One batch being validated: the snapshot it reads, and how far the changes
/// already checked have moved what it holds.
struct Validation<'a> {
    snapshot: &'a SessionSnapshot,
    prompts: Ledger<'a, Prompt, PromptStanding>,
    turns: Ledger<'a, Turn, TurnStanding<'a>>,
    messages: Ledger<'a, Message, MessageStanding>,
    activities: Ledger<'a, Activity, ActivityStanding>,
}

impl<'a> Validation<'a> {
    fn new(snapshot: &'a SessionSnapshot) -> Self {
        Self {
            snapshot,
            prompts: Ledger::new(&snapshot.prompts),
            turns: Ledger::new(&snapshot.turns),
            messages: Ledger::new(&snapshot.messages),
            activities: Ledger::new(&snapshot.activities),
        }
    }

    /// Checks one change against the Session as the batch has left it so far,
    /// and records how it moves it. Answers with the position of what the
    /// change names, where it names something the Session already holds.
    fn validate(&mut self, change: &'a SessionChange) -> Result<Option<usize>> {
        let target = match change {
            SessionChange::TitleChanged { .. }
            | SessionChange::AgentSelectionChanged { .. }
            | SessionChange::AgentSelectionAvailabilityChanged { .. }
            | SessionChange::ApprovalPostureChanged { .. }
            | SessionChange::AttachmentsDescribed { .. }
            | SessionChange::ContextFillChanged { .. }
            | SessionChange::SubagentInterventionsChanged { .. }
            | SessionChange::SubagentUsageChanged { .. }
            | SessionChange::TotalCostChanged { .. }
            | SessionChange::SessionWorkingChanged { .. }
            | SessionChange::SessionMonitoringChanged { .. }
            | SessionChange::SessionWatchesChanged { .. }
            | SessionChange::SessionWaitingOnSubagentsChanged { .. }
            | SessionChange::WorkspaceChanged { .. }
            | SessionChange::SessionStatusChanged { .. } => None,
            SessionChange::PromptAdded { prompt } => {
                if self.prompts.position(|held| held.id == prompt.id).is_some() {
                    bail!("Session update reused a Prompt identity");
                }
                if prompt.admission_order.0 == 0
                    || self
                        .prompts
                        .position(|held| held.admission_order == prompt.admission_order)
                        .is_some()
                {
                    bail!("Session update reused an invalid Prompt admission order");
                }
                self.prompts.add(prompt);
                None
            }
            SessionChange::PromptDeliveryChanged {
                prompt_id,
                delivery,
            } => {
                let Some((index, prompt)) = self.prompts.find(|prompt| prompt.id == *prompt_id)
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
                Some(index)
            }
            SessionChange::PromptStatusChanged { prompt_id, status } => {
                let Some((index, prompt)) = self.prompts.find(|prompt| prompt.id == *prompt_id)
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
                Some(index)
            }
            SessionChange::PromptWithdrawn { prompt_id, .. } => {
                let Some((index, prompt)) = self.prompts.find(|prompt| prompt.id == *prompt_id)
                else {
                    bail!("Session update referenced an unknown Prompt");
                };
                if prompt.status != PromptStatus::Pending {
                    bail!("Session update withdrew a Prompt that was not Pending");
                }
                prompt.status = PromptStatus::Cancelled;
                Some(index)
            }
            SessionChange::PromptTaken { prompt_id, .. } => {
                let Some((index, prompt)) = self.prompts.find(|prompt| prompt.id == *prompt_id)
                else {
                    bail!("Session update referenced an unknown Prompt");
                };
                if prompt.status != PromptStatus::Delivered || prompt.taken {
                    bail!("Session update took a Prompt not delivered, or taken already");
                }
                prompt.taken = true;
                Some(index)
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
                    && self
                        .prompts
                        .position(|prompt| prompt.id == prompt_id)
                        .is_none()
                {
                    bail!("Session update referenced an unknown Prompt");
                }
                if self.turns.position(|held| held.id == turn.id).is_some() {
                    bail!("Session update reused a Turn identity");
                }
                self.turns.add(turn);
                None
            }
            SessionChange::TurnAgentChanged { turn_id, agent } => {
                let Some((index, turn)) = self.turns.find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status != TurnStatus::Active {
                    bail!("Session update changed the Agent on a terminal Turn");
                }
                let Some(current) = turn.agent else {
                    bail!("Session update changed the Agent on an unbound Turn");
                };
                if current.agent != agent.agent
                    || current.selection.provider != agent.selection.provider
                {
                    bail!("Session update changed the Provider identity of an active Turn");
                }
                turn.agent = Some(agent);
                Some(index)
            }
            SessionChange::SubagentAgentChanged { turn_id, agent } => {
                if !self.snapshot.session.is_subagent() {
                    bail!("Session update observed a Subagent Agent on a root Session");
                }
                let Some((index, turn)) = self.turns.find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if let Some(current) = turn.agent
                    && (current.agent != agent.agent
                        || current.selection.provider != agent.selection.provider)
                {
                    bail!("Session update changed the Provider identity of a Subagent Turn");
                }
                turn.agent = Some(agent);
                Some(index)
            }
            SessionChange::TurnUsageChanged {
                turn_id,
                cost,
                cost_basis,
                cost_coverage,
                cost_recorded_at,
                ..
            } => {
                let Some((index, turn)) = self.turns.find(|turn| turn.id == *turn_id) else {
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
                Some(index)
            }
            SessionChange::TurnOutputObserved { turn_id, .. } => {
                let Some(index) = self.turns.position(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                Some(index)
            }
            SessionChange::MessageAdded { message } => {
                if self
                    .turns
                    .position(|turn| turn.id == message.turn_id)
                    .is_none()
                {
                    bail!("Session update referenced an unknown Turn");
                }
                if self
                    .messages
                    .position(|held| held.id == message.id)
                    .is_some()
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
                self.messages.add(message);
                None
            }
            SessionChange::MessageContentAppended { message_id, .. } => {
                let Some((index, message)) =
                    self.messages.find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if !message.from_agent || message.status != MessageStatus::Streaming {
                    bail!("Session update can only append to a streaming Agent Message");
                }
                if message.truncated {
                    bail!("Session update appended content past the cap that truncated a Message");
                }
                Some(index)
            }
            SessionChange::MessageTruncated { message_id } => {
                let Some((index, message)) =
                    self.messages.find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if !message.from_agent || message.status != MessageStatus::Streaming {
                    bail!("Session update can only truncate a streaming Agent Message");
                }
                message.truncated = true;
                Some(index)
            }
            SessionChange::MessageCompleted { message_id } => {
                let Some((index, message)) =
                    self.messages.find(|message| message.id == *message_id)
                else {
                    bail!("Session update referenced an unknown Message");
                };
                if !message.from_agent || message.status != MessageStatus::Streaming {
                    bail!("Session update can only complete a streaming Agent Message");
                }
                message.status = MessageStatus::Completed;
                Some(index)
            }
            SessionChange::DecisionAccepted { activity_id } => {
                let Some((
                    index,
                    ActivityStanding::Approval {
                        outcome, turn_id, ..
                    },
                )) = self
                    .activities
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if !outcome.is_answerable() || !self.turns.is_active(*turn_id) {
                    bail!("Approval is unavailable");
                }
                *outcome = ApprovalOutcome::Submitting;
                Some(index)
            }
            SessionChange::ApprovalSettled {
                activity_id,
                outcome,
                decision,
            } => {
                let Some((
                    index,
                    ActivityStanding::Approval {
                        outcome: current, ..
                    },
                )) = self
                    .activities
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if !current.is_live()
                    || (*outcome == ApprovalOutcome::SubmissionRejected
                        && (*current != ApprovalOutcome::Submitting || decision.is_some()))
                    || matches!(
                        outcome,
                        ApprovalOutcome::Pending | ApprovalOutcome::Submitting
                    )
                    || (*outcome == ApprovalOutcome::Decided && decision.is_none())
                    || (*outcome != ApprovalOutcome::Decided && decision.is_some())
                {
                    bail!("Approval is already unavailable");
                }
                *current = *outcome;
                Some(index)
            }
            SessionChange::ApprovalFollowUpFailed { activity_id, error } => {
                let Some((
                    index,
                    ActivityStanding::Approval {
                        outcome,
                        follow_up_failed,
                        ..
                    },
                )) = self
                    .activities
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Unknown Approval Activity");
                };
                if *outcome != ApprovalOutcome::Decided || *follow_up_failed || error.is_empty() {
                    bail!("Approval follow-up failure is invalid");
                }
                *follow_up_failed = true;
                Some(index)
            }
            SessionChange::QuestionnaireAccepted { activity_id } => {
                let Some((index, ActivityStanding::Questionnaire { outcome, turn_id })) = self
                    .activities
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Unknown Questionnaire Activity");
                };
                if !outcome.is_answerable() || !self.turns.is_active(*turn_id) {
                    bail!("Questionnaire is unavailable");
                }
                *outcome = QuestionnaireOutcome::Submitting;
                Some(index)
            }
            SessionChange::QuestionnaireSettled {
                activity_id,
                outcome,
                answer,
                ..
            } => {
                let Some((
                    index,
                    ActivityStanding::Questionnaire {
                        outcome: current, ..
                    },
                )) = self
                    .activities
                    .find(|activity| activity.id() == *activity_id)
                else {
                    bail!("Unknown Questionnaire Activity");
                };
                if !current.is_live()
                    || (*outcome == QuestionnaireOutcome::SubmissionRejected
                        && (*current != QuestionnaireOutcome::Submitting || answer.is_some()))
                    || matches!(
                        outcome,
                        QuestionnaireOutcome::Pending | QuestionnaireOutcome::Submitting
                    )
                {
                    bail!("Questionnaire is already unavailable");
                }
                *current = *outcome;
                Some(index)
            }
            SessionChange::ActivityAdded { activity } => {
                self.validate_added_activity(activity)?;
                self.activities.add(activity);
                None
            }
            SessionChange::CommandOutputAppended { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Command {
                    status,
                    output_truncated,
                } = standing
                else {
                    bail!("Session update appended command output to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended output to a terminal command Activity");
                }
                if *output_truncated {
                    bail!("Session update appended output past the cap that truncated a command");
                }
                Some(index)
            }
            SessionChange::CommandOutputTruncated { activity_id } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Command {
                    status,
                    output_truncated,
                } = standing
                else {
                    bail!("Session update truncated the output of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the output of a terminal command Activity");
                }
                *output_truncated = true;
                Some(index)
            }
            SessionChange::CommandStatusChanged {
                activity_id,
                status,
                ..
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Command {
                    status: current_status,
                    ..
                } = standing
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!("Session update contained an invalid command Activity status transition");
                }
                *current_status = *status;
                Some(index)
            }
            SessionChange::FileChangeUpdated { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::FileChange { status } = standing else {
                    bail!("Session update changed paths on a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update changed paths on a terminal file-change Activity");
                }
                Some(index)
            }
            SessionChange::FileChangeStatusChanged {
                activity_id,
                status,
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::FileChange {
                    status: current_status,
                } = standing
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!(
                        "Session update contained an invalid file-change Activity status transition"
                    );
                }
                *current_status = *status;
                Some(index)
            }
            SessionChange::ToolCallInputChanged { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::ToolCall { status, .. } = standing else {
                    bail!("Session update gave input to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update gave input to a terminal Tool Call Activity");
                }
                Some(index)
            }
            SessionChange::ToolCallOutputAppended { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::ToolCall {
                    status,
                    output_truncated,
                } = standing
                else {
                    bail!("Session update appended Tool Call output to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended output to a terminal Tool Call Activity");
                }
                if *output_truncated {
                    bail!("Session update appended output past the cap that truncated a Tool Call");
                }
                Some(index)
            }
            SessionChange::ToolCallOutputTruncated { activity_id } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::ToolCall {
                    status,
                    output_truncated,
                } = standing
                else {
                    bail!("Session update truncated the output of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the output of a terminal Tool Call Activity");
                }
                *output_truncated = true;
                Some(index)
            }
            SessionChange::ToolCallStatusChanged {
                activity_id,
                status,
                ..
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::ToolCall {
                    status: current_status,
                    ..
                } = standing
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!(
                        "Session update contained an invalid Tool Call Activity status transition"
                    );
                }
                *current_status = *status;
                Some(index)
            }
            SessionChange::ReasoningTitleChanged { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Reasoning { status, .. } = standing else {
                    bail!("Session update titled a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update titled a terminal Reasoning Activity");
                }
                Some(index)
            }
            SessionChange::ReasoningContentAppended { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Reasoning {
                    status,
                    content_truncated,
                } = standing
                else {
                    bail!("Session update appended Reasoning to a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update appended content to a terminal Reasoning Activity");
                }
                if *content_truncated {
                    bail!("Session update appended content past the cap that truncated Reasoning");
                }
                Some(index)
            }
            SessionChange::ReasoningContentTruncated { activity_id } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Reasoning {
                    status,
                    content_truncated,
                } = standing
                else {
                    bail!("Session update truncated the content of a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update truncated the content of a terminal Reasoning Activity");
                }
                *content_truncated = true;
                Some(index)
            }
            SessionChange::ReasoningStatusChanged {
                activity_id,
                status,
                ..
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Reasoning {
                    status: current_status,
                    ..
                } = standing
                else {
                    bail!("Session update completed a different Activity kind");
                };
                if *current_status != ActivityStatus::Active || *status == ActivityStatus::Active {
                    bail!(
                        "Session update contained an invalid Reasoning Activity status transition"
                    );
                }
                *current_status = *status;
                Some(index)
            }
            SessionChange::SubagentDescriptionChanged { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Subagent { status } = standing else {
                    bail!("Session update described a different Activity kind");
                };
                if *status != ActivityStatus::Active {
                    bail!("Session update described a terminal Subagent Activity");
                }
                Some(index)
            }
            SessionChange::SubagentModelChanged { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Subagent { .. } = standing else {
                    bail!("Session update identified a different Activity kind");
                };
                Some(index)
            }
            SessionChange::SubagentStatusChanged {
                activity_id,
                status,
                ..
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Subagent {
                    status: current_status,
                } = standing
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
                Some(index)
            }
            SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens,
                after_tokens,
                summary,
                summary_truncated,
                ..
            } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Compaction {
                    status: current_status,
                    after_measured,
                } = standing
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
                *after_measured = after_tokens.is_some();
                Some(index)
            }
            SessionChange::CompactionAfterMeasured { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Compaction {
                    status,
                    after_measured,
                } = standing
                else {
                    bail!("Session update measured a different Activity kind as a Compaction");
                };
                // Only a completed Compaction freed room a reading could
                // measure, and a count already known — the Provider's own
                // above all — is never replaced by one.
                if *status != ActivityStatus::Completed || *after_measured {
                    bail!("Session update measured a Compaction that takes no reading after it");
                }
                *after_measured = true;
                Some(index)
            }
            SessionChange::SubsessionTitleChanged { activity_id, .. } => {
                let (index, standing) = self.activity(activity_id)?;
                let ActivityStanding::Subsession = standing else {
                    bail!("Session update retitled a different Activity kind");
                };
                Some(index)
            }
            SessionChange::TurnStatusChanged {
                turn_id, status, ..
            } => {
                let Some((index, turn)) = self.turns.find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                if turn.status.is_terminal() || !status.is_terminal() {
                    bail!("Session update contained an invalid Turn status transition");
                }
                turn.status = *status;
                Some(index)
            }
        };
        Ok(target)
    }

    /// Resolves the Activity a change names, failing when the Session has no
    /// such Activity so every arm reports the same miss the same way and is
    /// left to check only that the Activity is the kind it can act on.
    fn activity(&mut self, activity_id: &ActivityId) -> Result<(usize, &mut ActivityStanding)> {
        let Some(found) = self
            .activities
            .find(|activity| activity.id() == *activity_id)
        else {
            bail!("Session update referenced an unknown Activity");
        };
        Ok(found)
    }

    /// Checks an Activity a change adds: that its Turn is one the Session
    /// holds, that it is new, and that it arrives in the initial state of its
    /// kind.
    fn validate_added_activity(&self, activity: &Activity) -> Result<()> {
        let Some(turn) = self
            .turns
            .position(|turn| turn.id == activity.turn_id())
            .map(|index| self.turns.entity(index))
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
        if self
            .activities
            .position(|held| held.id() == activity.id())
            .is_some()
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
        Ok(())
    }
}

/// One kind of thing a Session holds — its Prompts, Turns, Messages, or
/// Activities — as a batch being validated finds it: those the snapshot
/// holds, those the batch has added after them, and the standing of each the
/// batch has touched. Only what a later change could be refused over is
/// tracked, so a Message's content or a command's output is never copied.
struct Ledger<'a, T, S> {
    held: &'a [T],
    added: Vec<&'a T>,
    standings: HashMap<usize, S>,
}

impl<'a, T, S: From<&'a T>> Ledger<'a, T, S> {
    fn new(held: &'a [T]) -> Self {
        Self {
            held,
            added: Vec::new(),
            standings: HashMap::new(),
        }
    }

    /// Where the first entry `matches` stands once the batch so far is
    /// applied: among those held, or after them among those added.
    fn position(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.held.iter().position(&matches).or_else(|| {
            self.added
                .iter()
                .position(|added| matches(added))
                .map(|index| self.held.len() + index)
        })
    }

    /// The entry at `index` as it was held or added, for what about it no
    /// change can move.
    fn entity(&self, index: usize) -> &'a T {
        self.held
            .get(index)
            .unwrap_or_else(|| self.added[index - self.held.len()])
    }

    /// The standing of the first entry `matches`, as the batch so far has
    /// left it.
    fn find(&mut self, matches: impl Fn(&T) -> bool) -> Option<(usize, &mut S)> {
        let index = self.position(matches)?;
        let entity = self.entity(index);
        Some((
            index,
            self.standings
                .entry(index)
                .or_insert_with(|| S::from(entity)),
        ))
    }

    fn add(&mut self, entity: &'a T) {
        self.added.push(entity);
    }
}

impl<'a> Ledger<'a, Turn, TurnStanding<'a>> {
    /// Whether the Turn `turn_id` is at work, as the batch so far has left it.
    fn is_active(&mut self, turn_id: TurnId) -> bool {
        self.find(|turn| turn.id == turn_id)
            .is_some_and(|(_, turn)| turn.status == TurnStatus::Active)
    }
}

/// What a later change in the same batch could be refused over about a Prompt.
struct PromptStanding {
    status: PromptStatus,
    delivery: PromptDelivery,
    taken: bool,
}

impl From<&Prompt> for PromptStanding {
    fn from(prompt: &Prompt) -> Self {
        Self {
            status: prompt.status,
            delivery: prompt.delivery,
            taken: prompt.taken.is_some(),
        }
    }
}

/// What a later change in the same batch could be refused over about a Turn.
struct TurnStanding<'a> {
    status: TurnStatus,
    agent: Option<&'a AgentIdentity>,
}

impl<'a> From<&'a Turn> for TurnStanding<'a> {
    fn from(turn: &'a Turn) -> Self {
        Self {
            status: turn.status,
            agent: turn.agent.as_ref(),
        }
    }
}

/// What a later change in the same batch could be refused over about a
/// Message.
struct MessageStanding {
    from_agent: bool,
    status: MessageStatus,
    truncated: bool,
}

impl From<&Message> for MessageStanding {
    fn from(message: &Message) -> Self {
        Self {
            from_agent: message.role == MessageRole::Agent,
            status: message.status,
            truncated: message.truncated,
        }
    }
}

/// What a later change in the same batch could be refused over about an
/// Activity, by its kind.
enum ActivityStanding {
    Approval {
        outcome: ApprovalOutcome,
        turn_id: TurnId,
        follow_up_failed: bool,
    },
    Questionnaire {
        outcome: QuestionnaireOutcome,
        turn_id: TurnId,
    },
    Command {
        status: ActivityStatus,
        output_truncated: bool,
    },
    FileChange {
        status: ActivityStatus,
    },
    ToolCall {
        status: ActivityStatus,
        output_truncated: bool,
    },
    Reasoning {
        status: ActivityStatus,
        content_truncated: bool,
    },
    Subagent {
        status: ActivityStatus,
    },
    Compaction {
        status: ActivityStatus,
        after_measured: bool,
    },
    Subsession,
    /// A kind no change acts on once it is added.
    Other,
}

impl From<&Activity> for ActivityStanding {
    fn from(activity: &Activity) -> Self {
        match activity {
            Activity::Approval {
                outcome,
                turn_id,
                follow_up_error,
                ..
            } => Self::Approval {
                outcome: *outcome,
                turn_id: *turn_id,
                follow_up_failed: follow_up_error.is_some(),
            },
            Activity::Questionnaire {
                outcome, turn_id, ..
            } => Self::Questionnaire {
                outcome: *outcome,
                turn_id: *turn_id,
            },
            Activity::Command {
                status,
                output_truncated,
                ..
            } => Self::Command {
                status: *status,
                output_truncated: *output_truncated,
            },
            Activity::FileChange { status, .. } => Self::FileChange { status: *status },
            Activity::ToolCall {
                status,
                output_truncated,
                ..
            } => Self::ToolCall {
                status: *status,
                output_truncated: *output_truncated,
            },
            Activity::Reasoning {
                status,
                content_truncated,
                ..
            } => Self::Reasoning {
                status: *status,
                content_truncated: *content_truncated,
            },
            Activity::Subagent { status, .. } => Self::Subagent { status: *status },
            Activity::Compaction {
                status,
                after_tokens,
                ..
            } => Self::Compaction {
                status: *status,
                after_measured: after_tokens.is_some(),
            },
            Activity::Subsession { .. } => Self::Subsession,
            Activity::Status { .. } | Activity::Error { .. } | Activity::WatchOutcome { .. } => {
                Self::Other
            }
        }
    }
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
        Cost, CostBasis, MessageId, ModelAvailability, PromptId, PromptOrder, PromptWithdrawal,
        Session, SessionId, SessionStatus, SessionTimestamp, Usage, Workspace,
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
                begun_by: None,
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
            taken: None,
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

    /// An Agent Message streaming in a Turn no Prompt began, opened, streamed,
    /// and completed in one batch.
    fn streamed_in_one_batch(turn_id: TurnId, message_id: MessageId) -> Vec<SessionChange> {
        vec![
            SessionChange::TurnAdded {
                turn: active_continuation(turn_id),
            },
            SessionChange::MessageAdded {
                message: Message {
                    id: message_id,
                    turn_id,
                    role: MessageRole::Agent,
                    status: MessageStatus::Streaming,
                    content: String::new(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                    truncated: false,
                    author: None,
                },
            },
            SessionChange::MessageContentAppended {
                message_id,
                content: "Mapped".to_owned(),
            },
            SessionChange::MessageContentAppended {
                message_id,
                content: " the seams".to_owned(),
            },
            SessionChange::MessageCompleted { message_id },
        ]
    }

    #[test]
    fn each_change_in_a_batch_lands_where_the_changes_before_it_left_the_session() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let message_id = MessageId::new();
        let mut snapshot = empty_snapshot(session_id);

        apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes: streamed_in_one_batch(turn_id, message_id),
            },
        )
        .expect("a batch may stream into the Message it adds, in the Turn it adds");

        assert_eq!(snapshot.revision, SessionRevision(2));
        assert_eq!(snapshot.turns.len(), 1);
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].content, "Mapped the seams");
        assert_eq!(snapshot.messages[0].status, MessageStatus::Completed);
        assert_eq!(
            snapshot.transcript,
            vec![TranscriptItem::Message { message_id }]
        );
    }

    #[test]
    fn a_batch_refused_partway_leaves_the_snapshot_exactly_as_it_was() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let message_id = MessageId::new();
        let before = empty_snapshot(session_id);
        let mut changes = streamed_in_one_batch(turn_id, message_id);
        changes.push(SessionChange::MessageContentAppended {
            message_id,
            content: " and the tests".to_owned(),
        });
        let mut snapshot = before.clone();

        let refused = apply_update(
            &mut snapshot,
            &SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes,
            },
        )
        .expect_err("the batch already completed the Message it appends to");

        assert_eq!(
            refused.to_string(),
            "Session update can only append to a streaming Agent Message"
        );
        assert_eq!(
            snapshot, before,
            "the changes ahead of the refused one left nothing behind"
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
            taken: None,
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
            taken: None,
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
