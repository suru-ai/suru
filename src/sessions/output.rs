//! Admitting Agent output into a Session: the changes that carry one step of a
//! Provider stream, and the gate every such step passes through.

use anyhow::anyhow;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, MessageId, MessageRole, SessionChange, SessionId,
    SessionSnapshot, SessionUpdate, TurnId, TurnStatus,
};

use super::SessionStore;

impl SessionStore {
    /// Unlike streamed output, measurements can complete after settlement.
    /// The latest Turn and request sequence gate them independently of status.
    pub(crate) fn report_context_fill(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        report: crate::provider::ContextFillReport,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get_mut(&session_id) else {
            return Ok(());
        };
        if report.turn_id.is_some_and(|origin| origin != turn_id)
            || record.snapshot.turns.last().map(|turn| turn.id) != Some(turn_id)
            || record
                .context_fill_order
                .is_some_and(|(previous_turn, sequence)| {
                    previous_turn == turn_id && sequence >= report.sequence
                })
        {
            return Ok(());
        }
        let mut fill = report.fill;
        fill.capacity_tokens = fill.capacity_tokens.filter(|capacity| *capacity > 0);
        state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::ContextFillChanged {
                context_fill: Some(fill),
            }],
        )?;
        state
            .sessions
            .get_mut(&session_id)
            .expect("Session was just updated")
            .context_fill_order = Some((turn_id, report.sequence));
        Ok(())
    }

    pub(crate) fn publish_agent_output(
        &self,
        session_id: SessionId,
        change: SessionChange,
    ) -> anyhow::Result<SessionUpdate> {
        self.publish_agent_output_changes(session_id, vec![change])
    }

    /// Publishes Agent output whose changes describe one step of a Provider
    /// stream, as a single update so a client never observes content apart
    /// from the truncation that ended it. A step that produced no change at
    /// all leaves the Session where it stands, since committing nothing would
    /// still spend a revision on it.
    pub(crate) fn publish_agent_output_changes(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if changes.is_empty() {
                return Ok(SessionUpdate {
                    session_id,
                    revision: record.snapshot.revision,
                    changes,
                });
            }
            for change in &changes {
                match change {
                    // A Subagent row outlives its Turn's settle (ADR 0015), so
                    // its own lifecycle is gated on the row still being open
                    // rather than on the Turn it was spawned under.
                    SessionChange::SubagentDescriptionChanged { activity_id, .. }
                    | SessionChange::SubagentModelChanged { activity_id, .. }
                    | SessionChange::SubagentStatusChanged { activity_id, .. } => {
                        let activity = record
                            .snapshot
                            .activities
                            .iter()
                            .find(|activity| activity.id() == *activity_id)
                            .ok_or_else(|| {
                                anyhow!("Agent output referenced an unknown Activity")
                            })?;
                        let Activity::Subagent { status, .. } = activity else {
                            return Err(anyhow!(
                                "Subagent output referenced an Activity of another kind"
                            ));
                        };
                        if *status != ActivityStatus::Active {
                            return Err(anyhow!("Agent output referenced a settled Subagent"));
                        }
                    }
                    // A post-Decision action begins only after delivery is
                    // durable. Its result can therefore arrive after the
                    // native Turn has independently settled.
                    SessionChange::ApprovalFollowUpFailed { activity_id, .. } => {
                        if !record
                            .snapshot
                            .activities
                            .iter()
                            .any(|activity| activity.id() == *activity_id)
                        {
                            return Err(anyhow!(
                                "Agent output referenced an unknown Approval Activity"
                            ));
                        }
                    }
                    change => {
                        let turn_id = agent_output_turn_id(&record.snapshot, change)?;
                        let turn = record
                            .snapshot
                            .turns
                            .iter()
                            .find(|turn| turn.id == turn_id)
                            .ok_or_else(|| anyhow!("Agent output referenced an unknown Turn"))?;
                        if turn.status != TurnStatus::Active {
                            return Err(anyhow!("Agent output referenced a terminal Turn"));
                        }
                    }
                }
            }
        }
        state.commit(&self.storage, session_id, changes)
    }
}

/// The changes that carry one step of normalized Agent Message content into a
/// Session: the content the Provider sent, and the truncation if Suru's cap
/// ended the stream there. Both travel as data, so no client has to read a
/// marker back out of the content.
pub(crate) fn message_content_changes(
    message_id: MessageId,
    content: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText { content, truncated } = content;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::MessageContentAppended {
            message_id,
            content,
        });
    }
    if truncated {
        changes.push(SessionChange::MessageTruncated { message_id });
    }
    changes
}

/// The changes that carry one step of normalized command output into a
/// Session, on the same terms as [`message_content_changes`].
pub(crate) fn command_output_changes(
    activity_id: ActivityId,
    output: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText {
        content,
        truncated: output_truncated,
    } = output;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::CommandOutputAppended {
            activity_id,
            content,
        });
    }
    if output_truncated {
        changes.push(SessionChange::CommandOutputTruncated { activity_id });
    }
    changes
}

/// The changes that carry one step of normalized Tool Call output into a
/// Session, on the same terms as [`message_content_changes`].
pub(crate) fn tool_call_output_changes(
    activity_id: ActivityId,
    output: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText {
        content,
        truncated: output_truncated,
    } = output;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::ToolCallOutputAppended {
            activity_id,
            content,
        });
    }
    if output_truncated {
        changes.push(SessionChange::ToolCallOutputTruncated { activity_id });
    }
    changes
}

/// The changes that carry one step of normalized Reasoning content into a
/// Session, on the same terms as [`message_content_changes`].
pub(crate) fn reasoning_content_changes(
    activity_id: ActivityId,
    content: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText {
        content,
        truncated: content_truncated,
    } = content;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::ReasoningContentAppended {
            activity_id,
            content,
        });
    }
    if content_truncated {
        changes.push(SessionChange::ReasoningContentTruncated { activity_id });
    }
    changes
}

fn agent_output_turn_id(
    snapshot: &SessionSnapshot,
    change: &SessionChange,
) -> anyhow::Result<TurnId> {
    match change {
        SessionChange::TurnUsageChanged { turn_id, .. }
        | SessionChange::SubagentAgentChanged { turn_id, .. } => Ok(*turn_id),
        SessionChange::MessageAdded { message } if message.role == MessageRole::Agent => {
            Ok(message.turn_id)
        }
        SessionChange::MessageContentAppended { message_id, .. }
        | SessionChange::MessageTruncated { message_id }
        | SessionChange::MessageCompleted { message_id } => snapshot
            .messages
            .iter()
            .find(|message| message.id == *message_id && message.role == MessageRole::Agent)
            .map(|message| message.turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown Agent Message")),
        SessionChange::ActivityAdded { activity } => Ok(activity.turn_id()),
        SessionChange::DecisionAccepted { activity_id }
        | SessionChange::ApprovalSettled { activity_id, .. }
        | SessionChange::ApprovalFollowUpFailed { activity_id, .. }
        | SessionChange::QuestionnaireAccepted { activity_id }
        | SessionChange::QuestionnaireSettled { activity_id, .. }
        | SessionChange::CommandOutputAppended { activity_id, .. }
        | SessionChange::CommandOutputTruncated { activity_id }
        | SessionChange::CommandStatusChanged { activity_id, .. }
        | SessionChange::FileChangeUpdated { activity_id, .. }
        | SessionChange::FileChangeStatusChanged { activity_id, .. }
        | SessionChange::ToolCallInputChanged { activity_id, .. }
        | SessionChange::ToolCallOutputAppended { activity_id, .. }
        | SessionChange::ToolCallOutputTruncated { activity_id }
        | SessionChange::ToolCallStatusChanged { activity_id, .. }
        | SessionChange::ReasoningTitleChanged { activity_id, .. }
        | SessionChange::ReasoningContentAppended { activity_id, .. }
        | SessionChange::ReasoningContentTruncated { activity_id }
        | SessionChange::ReasoningStatusChanged { activity_id, .. } => snapshot
            .activities
            .iter()
            .find(|activity| activity.id() == *activity_id)
            .map(Activity::turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown Activity")),
        _ => Err(anyhow!("Session change is not Agent output")),
    }
}
