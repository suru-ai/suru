//! Asking whether the Attachments composer history brings back into a draft
//! are still stored. A recalled Prompt may bind an Attachment its Server has
//! since reclaimed (ADR 0037), so a recall shows the draft at once, bound as
//! it was sent, and the Application asks the run loop to check each
//! Attachment; the answer demotes to plain text the label of any the Server
//! no longer holds. This keeps which draft and which recalled bindings each
//! check still waiting was asked for.

use std::collections::HashMap;

use crate::protocol::AttachmentId;

use super::{composer::ComposerKey, text_binding::BoundAttachment};

/// One check of whether a recall's Attachments are still stored, so its
/// answer reaches the draft it was asked for.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AttachmentCheckId(u64);

impl Default for AttachmentCheckId {
    fn default() -> Self {
        Self(1)
    }
}

/// The checks still waiting on their answer.
#[derive(Clone, Debug, Default)]
pub(super) struct AttachmentChecks {
    next: AttachmentCheckId,
    pending: HashMap<AttachmentCheckId, PendingCheck>,
}

/// What a check was asked for: the draft the recall landed in, and the
/// Attachments it brought there, each with the label it stood as.
#[derive(Clone, Debug)]
pub(super) struct PendingCheck {
    pub(super) draft: ComposerKey,
    pub(super) recalled: Vec<BoundAttachment>,
}

impl PendingCheck {
    /// Whether `bound` is one of the recall's own bindings naming an
    /// Attachment in `missing`. A label pasted since, even of the same bytes,
    /// is its own and is never demoted by an answer about the recall.
    pub(super) fn demotes(&self, bound: &BoundAttachment, missing: &[AttachmentId]) -> bool {
        missing.contains(bound.attachment_id()) && self.recalled.contains(bound)
    }
}

impl AttachmentChecks {
    /// Starts a check of the Attachments `recalled` brought into `draft`,
    /// answering its id and each Attachment to ask about, once each.
    pub(super) fn begin(
        &mut self,
        draft: ComposerKey,
        recalled: Vec<BoundAttachment>,
    ) -> (AttachmentCheckId, Vec<AttachmentId>) {
        let check = self.next;
        self.next = AttachmentCheckId(check.0 + 1);
        let mut attachments = Vec::with_capacity(recalled.len());
        for bound in &recalled {
            if !attachments.contains(bound.attachment_id()) {
                attachments.push(bound.attachment_id().clone());
            }
        }
        self.pending.insert(check, PendingCheck { draft, recalled });
        (check, attachments)
    }

    /// Ends a check, answering what it was asked for.
    pub(super) fn finish(&mut self, check: AttachmentCheckId) -> Option<PendingCheck> {
        self.pending.remove(&check)
    }

    /// Forgets every check asked for `draft`, whose answers no longer speak
    /// for the Server that draft goes to.
    pub(super) fn forget(&mut self, draft: &ComposerKey) {
        self.pending.retain(|_, pending| &pending.draft != draft);
    }
}
