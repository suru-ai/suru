//! Attachments: media a user places on a Prompt beside its text, uploaded as
//! bytes before the Prompt is admitted and stored once, content-addressed, in
//! the Session database (ADR 0037). This module decides what an upload is,
//! whether a Prompt's bindings may stand, and what a Provider is handed of them
//! once that Prompt is delivered; storing and serving the bytes is the storage
//! repository's.
//!
//! Nothing counts references to an Attachment. An Attachment's age is measured
//! from when it was last referenced — uploaded, first or again, or bound by a
//! Prompt being admitted — and one younger than [`ATTACHMENT_GRACE`] is never
//! reclaimed, neither by a Session's deletion nor by the orphan sweep.
//! Admission stamps every Attachment its Prompt binds just before recording
//! the Prompt, so nothing can reclaim one between that moment and the flush
//! that joins it to the Prompt's Session.

mod image_header;

use std::{collections::HashSet, time::Duration};

use crate::{
    protocol::{AttachmentBinding, AttachmentDescriptor, AttachmentId, AttachmentKind, SessionId},
    provider::ProviderAttachment,
    storage::{StorageError, StorageRepository},
};

use image_header::HeaderError;

/// The most bytes one image may carry: the tightest limit any hosted Provider
/// publishes.
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// The most Attachments one Prompt may bind.
pub(crate) const MAX_ATTACHMENTS_PER_PROMPT: usize = 10;

/// How long after it was last uploaded or bound an Attachment is left alone,
/// whether or not any Session still references it.
pub(crate) const ATTACHMENT_GRACE: Duration = Duration::from_secs(60 * 60);

/// The most bytes the upload route reads: the per-image cap with headroom, so
/// an image a little over it is refused for its size, measured, rather than
/// cut off mid-read.
pub(crate) const UPLOAD_BODY_LIMIT: usize = MAX_ATTACHMENT_BYTES + 64 * 1024;

/// Why an upload was not stored, in words a client can show as they stand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UploadRefusal {
    /// Over the per-image cap; the length is absent where the body ran past
    /// what the upload route reads at all.
    TooLarge {
        byte_length: Option<usize>,
    },
    Unsupported(String),
}

impl UploadRefusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TooLarge {
                byte_length: Some(byte_length),
            } => format!(
                "An image may be at most {}, and this one is {}",
                mebibytes(MAX_ATTACHMENT_BYTES),
                mebibytes(*byte_length)
            ),
            Self::TooLarge { byte_length: None } => format!(
                "An image may be at most {}",
                mebibytes(MAX_ATTACHMENT_BYTES)
            ),
            Self::Unsupported(reason) => reason.clone(),
        }
    }
}

/// A length in mebibytes to the tenth, rounded up, so a length over a cap
/// never reads as the cap itself.
fn mebibytes(bytes: usize) -> String {
    let tenths = bytes.saturating_mul(10).div_ceil(1024 * 1024);
    match tenths % 10 {
        0 => format!("{} MiB", tenths / 10),
        tenth => format!("{}.{tenth} MiB", tenths / 10),
    }
}

/// Describes uploaded bytes as the Attachment they would be stored as, from
/// their size and header alone. The format is sniffed from the bytes; whatever
/// type a client declared never enters into it.
pub(crate) fn describe(bytes: &[u8]) -> Result<AttachmentDescriptor, UploadRefusal> {
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(UploadRefusal::TooLarge {
            byte_length: Some(bytes.len()),
        });
    }
    let header = image_header::read(bytes).map_err(|error| {
        UploadRefusal::Unsupported(match error {
            HeaderError::UnsupportedFormat => {
                "Only PNG, JPEG, GIF, and WebP images can be attached".to_owned()
            }
            HeaderError::Malformed(format) => {
                format!("This {} image's header could not be read", format.name())
            }
            HeaderError::UnsupportedJpegCoding => {
                "Only baseline and progressive JPEG images can be attached".to_owned()
            }
        })
    })?;
    Ok(AttachmentDescriptor {
        id: AttachmentId::of_bytes(bytes),
        kind: AttachmentKind::Image {
            width: header.width,
            height: header.height,
        },
        mime_type: header.format.mime_type().to_owned(),
        byte_length: bytes.len() as u64,
    })
}

/// Why a Prompt's Attachment bindings may not stand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BindingRefusal {
    TooMany { count: usize },
    Invalid(String),
    Unknown(AttachmentId),
}

impl BindingRefusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TooMany { count } => format!(
                "A Prompt may carry at most {MAX_ATTACHMENTS_PER_PROMPT} Attachments, and this one carries {count}"
            ),
            Self::Invalid(reason) => reason.clone(),
            Self::Unknown(id) => {
                format!("Attachment {id} is not stored on this Server; attach it again")
            }
        }
    }
}

/// Checks a Prompt's bindings against its own text: at most the per-Prompt
/// number of them, each a non-empty label lying on character boundaries
/// within the text and reading there exactly as bound, none overlapping
/// another.
pub(crate) fn check_bindings(
    text: &str,
    bindings: &[AttachmentBinding],
) -> Result<(), BindingRefusal> {
    if bindings.len() > MAX_ATTACHMENTS_PER_PROMPT {
        return Err(BindingRefusal::TooMany {
            count: bindings.len(),
        });
    }
    let mut spans = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let label = &binding.label;
        if label.is_empty() {
            return Err(BindingRefusal::Invalid(
                "An Attachment binding needs a label".to_owned(),
            ));
        }
        let range = binding.span.range();
        let Some(bound) = text.get(range.clone()) else {
            return Err(BindingRefusal::Invalid(format!(
                "Attachment label `{label}` lies outside the Prompt's text"
            )));
        };
        if bound != label {
            return Err(BindingRefusal::Invalid(format!(
                "The Prompt's text does not read `{label}` where that Attachment label is bound"
            )));
        }
        spans.push((range.start, range.end, label));
    }
    spans.sort_unstable();
    if let Some(pair) = spans.windows(2).find(|pair| pair[1].0 < pair[0].1) {
        return Err(BindingRefusal::Invalid(format!(
            "Attachment labels `{}` and `{}` overlap in the Prompt's text",
            pair[0].2, pair[1].2
        )));
    }
    Ok(())
}

/// The Attachments this Server has stored, as the upload, fetch, and
/// admission routes reach them, and as Provider delivery reads them.
#[derive(Clone)]
pub(crate) struct AttachmentStore {
    repository: StorageRepository,
}

/// An upload answered with its descriptor, and whether these bytes were
/// stored just now or already had been.
pub(crate) struct Uploaded {
    pub(crate) descriptor: AttachmentDescriptor,
    pub(crate) created: bool,
}

pub(crate) enum UploadError {
    Refused(UploadRefusal),
    Storage(StorageError),
}

pub(crate) enum PromptAttachmentError {
    Refused(BindingRefusal),
    Storage(StorageError),
}

impl AttachmentStore {
    pub(crate) fn new(repository: StorageRepository) -> Self {
        Self { repository }
    }

    /// Stores uploaded bytes once, whoever uploads them and however often.
    pub(crate) async fn upload(&self, bytes: Vec<u8>) -> Result<Uploaded, UploadError> {
        let descriptor = describe(&bytes).map_err(UploadError::Refused)?;
        let created = self
            .repository
            .store_attachment(descriptor.clone(), bytes)
            .await
            .map_err(UploadError::Storage)?;
        Ok(Uploaded {
            descriptor,
            created,
        })
    }

    /// An Attachment's bytes and the type they were sniffed as, where one is
    /// stored under that id.
    pub(crate) async fn fetch(
        &self,
        id: AttachmentId,
    ) -> Result<Option<(String, Vec<u8>)>, StorageError> {
        self.repository.attachment_bytes(id).await
    }

    /// What a Provider is handed of a Prompt's Attachments as that Prompt is
    /// delivered: the bytes of each one its bindings name, read now, in the
    /// order its label first stands in the text, so a label standing twice
    /// for the same Attachment is handed over once. An Attachment that can no
    /// longer be read — reclaimed since the Prompt was admitted — is passed
    /// over and reported to the Log by its id and `session_id`'s rather than
    /// failing the delivery, and its label stays in the text as it stands.
    /// The label is the user's own words, so the Log never carries it.
    pub(crate) async fn deliverable(
        &self,
        session_id: SessionId,
        bindings: &[AttachmentBinding],
    ) -> Vec<ProviderAttachment> {
        let mut ordered = bindings.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|binding| binding.span.start);
        let mut seen = HashSet::new();
        ordered.retain(|binding| seen.insert((&binding.attachment_id, &binding.label)));

        let mut delivered = Vec::with_capacity(ordered.len());
        for binding in ordered {
            match self.fetch(binding.attachment_id.clone()).await {
                Ok(Some((mime_type, bytes))) => delivered.push(ProviderAttachment {
                    label: binding.label.clone(),
                    mime_type,
                    bytes,
                }),
                Ok(None) => tracing::warn!(
                    %session_id,
                    attachment_id = %binding.attachment_id,
                    "an Attachment a delivered Prompt binds is no longer stored; \
                     the Provider is handed the Prompt without it"
                ),
                Err(error) => tracing::error!(
                    %session_id,
                    attachment_id = %binding.attachment_id,
                    "an Attachment a delivered Prompt binds could not be read, \
                     so the Provider is handed the Prompt without it: {error}"
                ),
            }
        }
        delivered
    }

    /// Whether a Prompt's bindings may be admitted: sound against its text,
    /// and each naming an Attachment this Server has stored. Reads only.
    pub(crate) async fn check_prompt(
        &self,
        text: &str,
        bindings: &[AttachmentBinding],
    ) -> Result<(), PromptAttachmentError> {
        check_bindings(text, bindings).map_err(PromptAttachmentError::Refused)?;
        if bindings.is_empty() {
            return Ok(());
        }
        let stored = self
            .repository
            .stored_attachments(named(bindings))
            .await
            .map_err(PromptAttachmentError::Storage)?;
        refuse_unknown(bindings, |id| stored.contains(id))
    }

    /// As [`Self::check_prompt`], stamping every Attachment the bindings name
    /// as referenced now, for the moment right before the Prompt is recorded,
    /// and answering what each was described as when it was uploaded, once
    /// each and in id order, for the Session to describe beside the Prompt.
    pub(crate) async fn reference_prompt(
        &self,
        text: &str,
        bindings: &[AttachmentBinding],
    ) -> Result<Vec<AttachmentDescriptor>, PromptAttachmentError> {
        check_bindings(text, bindings).map_err(PromptAttachmentError::Refused)?;
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let described = self
            .repository
            .reference_attachments(named(bindings))
            .await
            .map_err(PromptAttachmentError::Storage)?;
        refuse_unknown(bindings, |id| {
            described.iter().any(|descriptor| &descriptor.id == id)
        })?;
        Ok(described)
    }
}

/// Each Attachment the bindings name, once.
fn named(bindings: &[AttachmentBinding]) -> Vec<AttachmentId> {
    bindings
        .iter()
        .map(|binding| binding.attachment_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// Refuses the first binding naming an Attachment that is not `stored`.
fn refuse_unknown(
    bindings: &[AttachmentBinding],
    stored: impl Fn(&AttachmentId) -> bool,
) -> Result<(), PromptAttachmentError> {
    match bindings
        .iter()
        .find(|binding| !stored(&binding.attachment_id))
    {
        Some(unknown) => Err(PromptAttachmentError::Refused(BindingRefusal::Unknown(
            unknown.attachment_id.clone(),
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TextSpan;

    fn binding(label: &str, start: u32, end: u32) -> AttachmentBinding {
        AttachmentBinding {
            attachment_id: AttachmentId::new("fixture"),
            label: label.to_owned(),
            span: TextSpan { start, end },
        }
    }

    fn one_pixel_gif() -> Vec<u8> {
        b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec()
    }

    #[test]
    fn an_upload_is_described_by_its_hash_sniffed_type_length_and_pixels() {
        let bytes = one_pixel_gif();
        assert_eq!(
            describe(&bytes),
            Ok(AttachmentDescriptor {
                id: AttachmentId::new(blake3::hash(&bytes).to_hex().to_string()),
                kind: AttachmentKind::Image {
                    width: 1,
                    height: 1
                },
                mime_type: "image/gif".to_owned(),
                byte_length: 13,
            })
        );
    }

    #[test]
    fn an_upload_over_the_cap_is_refused_by_its_measured_size() {
        let mut bytes = one_pixel_gif();
        bytes.resize(MAX_ATTACHMENT_BYTES, 0);
        assert!(describe(&bytes).is_ok(), "exactly the cap is admitted");
        bytes.push(0);
        let refusal = describe(&bytes).expect_err("one byte over the cap is refused");
        assert_eq!(
            refusal,
            UploadRefusal::TooLarge {
                byte_length: Some(MAX_ATTACHMENT_BYTES + 1)
            }
        );
        assert_eq!(
            refusal.message(),
            "An image may be at most 5 MiB, and this one is 5.1 MiB"
        );
        assert_eq!(
            UploadRefusal::TooLarge {
                byte_length: Some(7 * 1024 * 1024 + 512 * 1024)
            }
            .message(),
            "An image may be at most 5 MiB, and this one is 7.5 MiB"
        );
    }

    #[test]
    fn an_upload_of_another_format_is_refused_with_a_reason_to_show() {
        assert_eq!(
            describe(b"%PDF-1.7\n").map_err(|refusal| refusal.message()),
            Err("Only PNG, JPEG, GIF, and WebP images can be attached".to_owned())
        );
        assert_eq!(
            describe(b"GIF89a\x01").map_err(|refusal| refusal.message()),
            Err("This GIF image's header could not be read".to_owned())
        );
    }

    #[test]
    fn a_prompt_that_is_only_a_label_binds_it() {
        assert_eq!(
            check_bindings("[Image 1]", &[binding("[Image 1]", 0, 9)]),
            Ok(())
        );
    }

    #[test]
    fn labels_bind_on_character_boundaries_anywhere_in_the_text() {
        let text = "Compare é [Image 1] with [Image 2]";
        let first = text.find("[Image 1]").unwrap() as u32;
        let second = text.find("[Image 2]").unwrap() as u32;
        assert_eq!(
            check_bindings(
                text,
                &[
                    binding("[Image 2]", second, second + 9),
                    binding("[Image 1]", first, first + 9),
                ]
            ),
            Ok(())
        );
    }

    #[test]
    fn a_span_outside_the_text_or_off_a_character_boundary_is_refused() {
        let outside = "Attachment label `[Image 1]` lies outside the Prompt's text".to_owned();
        for (text, start, end) in [
            ("Look [Image 1", 5, 14),
            ("[Image 1]", 9, 0),
            ("é[Image 1]", 1, 10),
        ] {
            assert_eq!(
                check_bindings(text, &[binding("[Image 1]", start, end)]),
                Err(BindingRefusal::Invalid(outside.clone())),
                "{text:?} at {start}..{end}"
            );
        }
    }

    #[test]
    fn a_label_the_text_does_not_carry_at_its_span_is_refused() {
        assert_eq!(
            check_bindings("Look at [Image 2]", &[binding("[Image 1]", 8, 17)]),
            Err(BindingRefusal::Invalid(
                "The Prompt's text does not read `[Image 1]` where that Attachment label is bound"
                    .to_owned()
            ))
        );
        assert_eq!(
            check_bindings("Look", &[binding("", 0, 0)]),
            Err(BindingRefusal::Invalid(
                "An Attachment binding needs a label".to_owned()
            ))
        );
    }

    #[test]
    fn overlapping_labels_are_refused() {
        assert_eq!(
            check_bindings(
                "[Image 1]",
                &[binding("[Image 1]", 0, 9), binding("Image", 1, 6)]
            ),
            Err(BindingRefusal::Invalid(
                "Attachment labels `[Image 1]` and `Image` overlap in the Prompt's text".to_owned()
            ))
        );
    }

    #[test]
    fn more_than_ten_bindings_are_refused_before_any_is_read() {
        let text = "[Image 1]".repeat(11);
        let bindings = (0..11)
            .map(|index| binding("[Image 1]", index * 9, index * 9 + 9))
            .collect::<Vec<_>>();
        assert_eq!(check_bindings(&text, &bindings[..10]), Ok(()));
        let refusal = check_bindings(&text, &bindings).expect_err("eleven are too many");
        assert_eq!(refusal, BindingRefusal::TooMany { count: 11 });
        assert_eq!(
            refusal.message(),
            "A Prompt may carry at most 10 Attachments, and this one carries 11"
        );
    }

    #[tokio::test]
    async fn delivery_hands_over_each_stored_attachment_once_in_label_order_and_logs_a_reclaimed_one()
     {
        let directory = tempfile::tempdir().expect("create data directory");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open repository");
        let store = AttachmentStore::new(repository);
        let bytes = one_pixel_gif();
        let Ok(stored) = store.upload(bytes.clone()).await else {
            panic!("store the fixture");
        };
        let reclaimed = AttachmentId::new("reclaimed-since-admission");
        let session_id = SessionId::new();
        let text = "[Image 1] then [Image 2], [Image 1] again";
        let bound = |id: &AttachmentId, label: &str, start: usize| AttachmentBinding {
            attachment_id: id.clone(),
            label: label.to_owned(),
            span: TextSpan::from(start..start + label.len()),
        };
        let bindings = [
            bound(
                &stored.descriptor.id,
                "[Image 1]",
                text.rfind("[Image 1]").unwrap(),
            ),
            bound(&reclaimed, "[Image 2]", text.find("[Image 2]").unwrap()),
            bound(&stored.descriptor.id, "[Image 1]", 0),
        ];

        let log_path = directory.path().join("delivery.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(std::sync::Arc::new(
                std::fs::File::create(&log_path).expect("create the Log"),
            ))
            .finish();
        let delivered = {
            let _log = tracing::subscriber::set_default(subscriber);
            store.deliverable(session_id, &bindings).await
        };

        assert_eq!(
            delivered,
            [ProviderAttachment {
                label: "[Image 1]".to_owned(),
                mime_type: "image/gif".to_owned(),
                bytes,
            }],
            "the stored Attachment is handed over once, and the reclaimed one not at all"
        );
        let log = std::fs::read_to_string(&log_path).expect("read the Log");
        assert!(
            log.contains("WARN")
                && log.contains("attachment_id=reclaimed-since-admission")
                && log.contains(&format!("session_id={session_id}")),
            "the reclaimed Attachment is reported to the Log by its id and its Session's: {log}"
        );
        assert!(
            !log.contains("[Image"),
            "a label is the user's words, which the Log never carries: {log}"
        );
    }
}
