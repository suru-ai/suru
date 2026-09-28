//! Pasting from the host clipboard into a composer. A paste is a read of the
//! clipboard and then, for an image, an upload to the Server the draft's
//! Prompt will go to; only an upload that succeeds writes an `[Image N]` label
//! into the draft. The Application asks the run loop for each step and answers
//! what comes back, so this keeps which draft each paste in flight belongs to,
//! and words why a paste that failed inserted nothing.

use std::collections::HashMap;

use crate::attachments::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_PROMPT, UploadRefusal};

use super::{clipboard::PasteId, composer::ComposerKey, notice::PasteFailure};

/// The pastes still waiting on a read or an upload.
#[derive(Clone, Debug, Default)]
pub(super) struct ClipboardPastes {
    next: PasteId,
    pending: HashMap<PasteId, PendingPaste>,
}

#[derive(Clone, Debug)]
struct PendingPaste {
    draft: ComposerKey,
    uploading: bool,
}

impl ClipboardPastes {
    /// Starts a paste into `draft`, named by the id its answers carry.
    pub(super) fn begin(&mut self, draft: ComposerKey) -> PasteId {
        let paste = self.next;
        self.next = paste.after();
        self.pending.insert(
            paste,
            PendingPaste {
                draft,
                uploading: false,
            },
        );
        paste
    }

    /// The draft a paste still waiting belongs to.
    pub(super) fn draft(&self, paste: PasteId) -> Option<&ComposerKey> {
        self.pending.get(&paste).map(|pending| &pending.draft)
    }

    /// Marks a paste as waiting on its upload, which counts it against the
    /// draft's Attachments until it lands.
    pub(super) fn begin_upload(&mut self, paste: PasteId) {
        if let Some(pending) = self.pending.get_mut(&paste) {
            pending.uploading = true;
        }
    }

    /// Ends a paste, answering the draft it belonged to.
    pub(super) fn finish(&mut self, paste: PasteId) -> Option<ComposerKey> {
        self.pending.remove(&paste).map(|pending| pending.draft)
    }

    /// How many uploads are on their way into `draft`.
    pub(super) fn uploads_into(&self, draft: &ComposerKey) -> usize {
        self.pending
            .values()
            .filter(|pending| pending.uploading && &pending.draft == draft)
            .count()
    }
}

/// Why an image read from the clipboard may not be uploaded into a draft
/// already binding `attached` Attachments, with `uploading` more on their way.
pub(super) fn refuse_image(
    png: &[u8],
    attached: usize,
    uploading: usize,
) -> Option<(PasteFailure, String)> {
    if attached + uploading >= MAX_ATTACHMENTS_PER_PROMPT {
        return Some(too_many());
    }
    (png.len() > MAX_ATTACHMENT_BYTES).then(|| {
        (
            PasteFailure::TooLarge,
            UploadRefusal::TooLarge {
                byte_length: Some(png.len()),
            }
            .message(),
        )
    })
}

/// Why an upload that succeeded may still not be bound into a draft.
pub(super) fn refuse_binding(attached: usize) -> Option<(PasteFailure, String)> {
    (attached >= MAX_ATTACHMENTS_PER_PROMPT).then(too_many)
}

fn too_many() -> (PasteFailure, String) {
    (
        PasteFailure::TooMany,
        format!("A Prompt may carry at most {MAX_ATTACHMENTS_PER_PROMPT} Attachments"),
    )
}

pub(super) fn unreadable(reason: &str) -> (PasteFailure, String) {
    (
        PasteFailure::Unreadable,
        format!(
            "Could not read the clipboard: {}",
            reason.trim().trim_end_matches('.')
        ),
    )
}

pub(super) fn unsupported(format: &str) -> (PasteFailure, String) {
    (
        PasteFailure::UnsupportedFormat,
        format!("The clipboard's {format} image cannot be attached"),
    )
}

pub(super) fn refused(reason: String) -> (PasteFailure, String) {
    (PasteFailure::Refused, reason)
}
