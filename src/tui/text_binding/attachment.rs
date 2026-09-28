//! The Attachment as a kind of text binding: an uploaded Attachment bound to
//! the `[Image N]` label that stands for it in a Prompt's text. A label is one
//! unit of the text it stands in, which no Skill Invocation is.

use std::ops::Range;

use crate::protocol::{AttachmentBinding, AttachmentDescriptor, AttachmentId, TextSpan};

use super::{BindingKind, TextBinding, TextBindings};

/// The Attachment a span of text is bound to, and the label it is written as
/// there.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::tui) struct BoundAttachment {
    attachment_id: AttachmentId,
    label: String,
}

impl BoundAttachment {
    /// The binding a Prompt's Attachment binding is raised to.
    pub(super) fn binding(binding: &AttachmentBinding) -> TextBinding {
        TextBinding {
            span: binding.span.range(),
            kind: BindingKind::Attachment(Self {
                attachment_id: binding.attachment_id.clone(),
                label: binding.label.clone(),
            }),
        }
    }

    /// The Attachment binding this lowers to at `span`.
    pub(super) fn attachment_binding(&self, span: &Range<usize>) -> AttachmentBinding {
        AttachmentBinding {
            attachment_id: self.attachment_id.clone(),
            label: self.label.clone(),
            span: TextSpan::from(span.clone()),
        }
    }

    pub(super) fn is_written_as(&self, text: &str) -> bool {
        text == self.label
    }

    pub(in crate::tui) fn attachment_id(&self) -> &AttachmentId {
        &self.attachment_id
    }

    pub(in crate::tui) fn label(&self) -> &str {
        &self.label
    }
}

impl TextBindings {
    /// Binds the uploaded Attachment `attachment_id` to the `label` written at
    /// `span`.
    pub(in crate::tui) fn bind_attachment(
        &mut self,
        span: Range<usize>,
        attachment_id: AttachmentId,
        label: String,
    ) {
        self.bind(
            span,
            BindingKind::Attachment(BoundAttachment {
                attachment_id,
                label,
            }),
        );
    }

    /// Each Attachment bound beside the text, in text order.
    pub(in crate::tui) fn attachments(
        &self,
    ) -> impl Iterator<Item = (&Range<usize>, &BoundAttachment)> {
        self.0.iter().filter_map(TextBinding::attachment)
    }

    /// The highest `N` among the `[Image N]` labels bound beside the text, or
    /// zero with none.
    pub(in crate::tui) fn highest_image_number(&self) -> u32 {
        self.attachments()
            .filter_map(|(_, attachment)| image_label_number(&attachment.label))
            .max()
            .unwrap_or(0)
    }
}

/// The label the `number`th image pasted into a draft stands in its text as.
pub(in crate::tui) fn image_label(number: u32) -> String {
    format!("[Image {number}]")
}

fn image_label_number(label: &str) -> Option<u32> {
    label
        .strip_prefix("[Image ")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// The one line that describes an Attachment beneath the text its label
/// stands in: the label without its brackets, the format, the dimensions, and
/// the size, as in `Image 1 · PNG · 1280×720 · 312 KiB`.
pub(in crate::tui) fn attachment_line(label: &str, descriptor: &AttachmentDescriptor) -> String {
    let name = label
        .strip_prefix('[')
        .and_then(|label| label.strip_suffix(']'))
        .unwrap_or(label);
    let crate::protocol::AttachmentKind::Image { width, height } = descriptor.kind;
    format!(
        "{name} · {} · {width}×{height} · {}",
        format_name(&descriptor.mime_type),
        byte_size(descriptor.byte_length)
    )
}

/// The name a reader knows an Attachment's format by, from its type.
fn format_name(mime_type: &str) -> String {
    match mime_type {
        "image/png" => "PNG".to_owned(),
        "image/jpeg" => "JPEG".to_owned(),
        "image/gif" => "GIF".to_owned(),
        "image/webp" => "WebP".to_owned(),
        other => other
            .rsplit_once('/')
            .map_or(other, |(_, subtype)| subtype)
            .to_uppercase(),
    }
}

/// A length in the largest binary unit it reaches, to a tenth below ten of
/// that unit and whole above it.
fn byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes_f = bytes as f64;
    let (value, unit) = if bytes < 1024 {
        return format!("{bytes} B");
    } else if bytes_f < KIB * KIB {
        (bytes_f / KIB, "KiB")
    } else {
        (bytes_f / (KIB * KIB), "MiB")
    };
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AttachmentKind;

    fn descriptor(
        mime_type: &str,
        width: u32,
        height: u32,
        byte_length: u64,
    ) -> AttachmentDescriptor {
        AttachmentDescriptor {
            id: AttachmentId::new("id"),
            kind: AttachmentKind::Image { width, height },
            mime_type: mime_type.to_owned(),
            byte_length,
        }
    }

    #[test]
    fn an_attachment_line_reads_its_label_format_dimensions_and_size() {
        assert_eq!(
            attachment_line("[Image 1]", &descriptor("image/png", 1280, 720, 312 * 1024)),
            "Image 1 · PNG · 1280×720 · 312 KiB"
        );
        assert_eq!(
            attachment_line(
                "[Image 12]",
                &descriptor("image/jpeg", 4032, 3024, 2_411_725)
            ),
            "Image 12 · JPEG · 4032×3024 · 2.3 MiB"
        );
        assert_eq!(
            attachment_line("[Image 2]", &descriptor("image/webp", 16, 16, 900)),
            "Image 2 · WebP · 16×16 · 900 B"
        );
        assert_eq!(
            attachment_line("[Image 3]", &descriptor("image/gif", 1, 1, 4608)),
            "Image 3 · GIF · 1×1 · 4.5 KiB"
        );
        assert_eq!(
            attachment_line(
                "[Image 4]",
                &descriptor("image/avif", 2, 2, 11 * 1024 * 1024)
            ),
            "Image 4 · AVIF · 2×2 · 11 MiB"
        );
    }

    #[test]
    fn image_labels_are_numbered_and_read_back() {
        assert_eq!(image_label(3), "[Image 3]");
        assert_eq!(image_label_number("[Image 3]"), Some(3));
        assert_eq!(image_label_number("[Image three]"), None);
        assert_eq!(image_label_number("Image 3"), None);
    }
}
