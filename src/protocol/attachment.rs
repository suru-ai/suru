//! Attachments on the wire: what a client learns of one it uploaded, and how
//! a Prompt binds one to a label in its text. Neither ever carries the bytes;
//! a client that wants them fetches them by id (ADR 0037).

use super::TextSpan;
use serde::{Deserialize, Serialize};
use std::fmt;

/// An Attachment's identity: the lowercase hex blake3 hash of its bytes, so
/// the same bytes uploaded twice are the same Attachment.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AttachmentId(String);

impl AttachmentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identity the given bytes are stored under.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// What an Attachment is, with the facts only that kind has. An image is the
/// only kind today; another kind is another variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachmentKind {
    /// A PNG, JPEG, GIF, or WebP image, measured in pixels from its header.
    Image { width: u32, height: u32 },
}

/// Everything a client learns of a stored Attachment short of its bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentDescriptor {
    pub id: AttachmentId,
    pub kind: AttachmentKind,
    /// The type the Server sniffed from the bytes themselves, never the one a
    /// client declared on upload.
    pub mime_type: String,
    pub byte_length: u64,
}

/// A binding between a label in a Prompt's text, such as `[Image 1]`, and the
/// stored Attachment it stands for, carried beside the text as a Skill
/// Invocation is. Repeated labels remain separate bindings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentBinding {
    pub attachment_id: AttachmentId,
    /// The label exactly as it stands in the text at `span`.
    pub label: String,
    pub span: TextSpan,
}
