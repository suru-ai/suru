//! Shared projection rules for authoritative and client-side Session state.

use anyhow::{Result, bail};

use crate::protocol::{
    MessageRole, MessageStatus, PromptDelivery, PromptStatus, SessionChange, SessionSnapshot,
    SessionUpdate, TranscriptItem,
};

pub(crate) fn apply_update(snapshot: &mut SessionSnapshot, update: &SessionUpdate) -> Result<()> {
    if snapshot.session.id != update.session_id {
        bail!("Session update targeted a different Session");
    }
    if !update.revision.immediately_follows(snapshot.revision) {
        bail!("Session update revision is not monotonic");
    }

    let mut next = snapshot.clone();
    for change in &update.changes {
        match change {
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
                    || !matches!(status, PromptStatus::Delivered | PromptStatus::Cancelled)
                {
                    bail!("Session update contained an invalid Prompt status transition");
                }
                prompt.status = *status;
            }
            SessionChange::TurnAdded { turn } => {
                if !next
                    .prompts
                    .iter()
                    .any(|prompt| prompt.id == turn.prompt_id)
                {
                    bail!("Session update referenced an unknown Prompt");
                }
                if next.turns.iter().any(|existing| existing.id == turn.id) {
                    bail!("Session update reused a Turn identity");
                }
                next.turns.push(turn.clone());
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
                message.content.push_str(content);
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
            SessionChange::ActivityAdded { activity } => {
                if !next.turns.iter().any(|turn| turn.id == activity.turn_id) {
                    bail!("Session update referenced an unknown Turn");
                }
                if next
                    .activities
                    .iter()
                    .any(|existing| existing.id == activity.id)
                {
                    bail!("Session update reused an Activity identity");
                }
                next.activities.push(activity.clone());
                next.transcript.push(TranscriptItem::Activity {
                    activity_id: activity.id,
                });
            }
            SessionChange::TurnStatusChanged { turn_id, status } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    bail!("Session update referenced an unknown Turn");
                };
                turn.status = *status;
            }
            SessionChange::SessionStatusChanged { status } => next.session.status = *status,
        }
    }
    next.revision = update.revision;
    *snapshot = next;
    Ok(())
}
