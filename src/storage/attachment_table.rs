//! Attachment rows: written when their bytes are first uploaded and stamped
//! again on every later upload of the same bytes, joined to the Sessions whose
//! stored Prompts and Messages bind them, and read back only when something
//! asks for the bytes (ADR 0037). An Attachment's age, which decides whether
//! it may be reclaimed, is measured from its last upload.

use std::collections::HashSet;

use diesel::{SqliteConnection, dsl::exists, prelude::*};

use crate::protocol::{AttachmentDescriptor, AttachmentId, AttachmentKind, SessionTimestamp};

use super::{StorageError, StorageRepository, attachments, on_blocking_task, session_attachments};

#[derive(Insertable)]
#[diesel(table_name = attachments)]
struct AttachmentRow {
    id: String,
    mime_type: String,
    byte_length: i64,
    width: Option<i64>,
    height: Option<i64>,
    uploaded_at: i64,
    bytes: Vec<u8>,
}

/// A moment as the `uploaded_at` column stores it.
pub(super) fn millis(moment: SessionTimestamp) -> i64 {
    i64::try_from(moment.0).unwrap_or(i64::MAX)
}

impl StorageRepository {
    /// Stores an upload's bytes under its descriptor, or where the same bytes
    /// already are, stamps them uploaded again now; answers whether this call
    /// stored them.
    pub(crate) async fn store_attachment(
        &self,
        descriptor: AttachmentDescriptor,
        bytes: Vec<u8>,
    ) -> Result<bool, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("store Attachment", move || {
            let AttachmentKind::Image { width, height } = descriptor.kind;
            let row = AttachmentRow {
                id: descriptor.id.as_str().to_owned(),
                mime_type: descriptor.mime_type,
                byte_length: i64::try_from(descriptor.byte_length)
                    .map_err(|error| StorageError::WriteAttachment(error.to_string()))?,
                width: Some(width.into()),
                height: Some(height.into()),
                uploaded_at: millis(SessionTimestamp::now()),
                bytes,
            };
            let mut connection = super::connect(&path)?;
            connection
                .transaction::<_, diesel::result::Error, _>(|connection| {
                    let inserted = diesel::insert_into(attachments::table)
                        .values(&row)
                        .on_conflict(attachments::id)
                        .do_nothing()
                        .execute(connection)?;
                    if inserted == 0 {
                        diesel::update(attachments::table.filter(attachments::id.eq(&row.id)))
                            .set(attachments::uploaded_at.eq(row.uploaded_at))
                            .execute(connection)?;
                    }
                    Ok(inserted == 1)
                })
                .map_err(|error| StorageError::WriteAttachment(error.to_string()))
        })
        .await
    }

    /// An Attachment's sniffed type and bytes, where one is stored under
    /// that id.
    pub(crate) async fn attachment_bytes(
        &self,
        id: AttachmentId,
    ) -> Result<Option<(String, Vec<u8>)>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("read Attachment", move || {
            let mut connection = super::connect(&path)?;
            attachments::table
                .filter(attachments::id.eq(id.as_str()))
                .select((attachments::mime_type, attachments::bytes))
                .first::<(String, Vec<u8>)>(&mut connection)
                .optional()
                .map_err(|error| StorageError::Read(error.to_string()))
        })
        .await
    }

    /// Which of the named Attachments are stored, without reading their
    /// bytes.
    pub(crate) async fn stored_attachments(
        &self,
        ids: Vec<AttachmentId>,
    ) -> Result<HashSet<AttachmentId>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("find Attachments", move || {
            let mut connection = super::connect(&path)?;
            let stored = attachments::table
                .filter(attachments::id.eq_any(ids.iter().map(AttachmentId::as_str)))
                .select(attachments::id)
                .load::<String>(&mut connection)
                .map_err(|error| StorageError::Read(error.to_string()))?;
            Ok(stored.into_iter().map(AttachmentId::new).collect())
        })
        .await
    }
}

/// Joins a Session to every Attachment its stored Prompts and Messages bind,
/// inside the transaction that writes those rows. A binding whose Attachment
/// is no longer stored is reported to the Log and passed over rather than
/// failing the Session's own write.
pub(super) fn join_session_attachments(
    connection: &mut SqliteConnection,
    session_id: &str,
    attachment_ids: &[String],
) -> Result<(), diesel::result::Error> {
    if attachment_ids.is_empty() {
        return Ok(());
    }
    let stored = attachments::table
        .filter(attachments::id.eq_any(attachment_ids))
        .select(attachments::id)
        .load::<String>(connection)?
        .into_iter()
        .collect::<HashSet<_>>();
    for attachment_id in attachment_ids {
        if !stored.contains(attachment_id) {
            tracing::warn!(
                session_id,
                attachment_id,
                "a Session binds an Attachment that is no longer stored"
            );
            continue;
        }
        diesel::insert_or_ignore_into(session_attachments::table)
            .values((
                session_attachments::session_id.eq(session_id),
                session_attachments::attachment_id.eq(attachment_id),
            ))
            .execute(connection)?;
    }
    Ok(())
}

/// The Attachments a Session is joined to, read before its deletion takes the
/// joins with it.
pub(super) fn session_attachment_ids(
    connection: &mut SqliteConnection,
    session_id: &str,
) -> Result<Vec<String>, diesel::result::Error> {
    session_attachments::table
        .filter(session_attachments::session_id.eq(session_id))
        .select(session_attachments::attachment_id)
        .load(connection)
}

/// Deletes those of the given Attachments that no Session is joined to any
/// longer and that were last uploaded no later than `uploaded_before`. An
/// upload no Session has bound yet is never among them.
pub(super) fn delete_unjoined_attachments(
    connection: &mut SqliteConnection,
    attachment_ids: &[String],
    uploaded_before: i64,
) -> Result<(), diesel::result::Error> {
    if attachment_ids.is_empty() {
        return Ok(());
    }
    diesel::delete(
        attachments::table
            .filter(attachments::id.eq_any(attachment_ids))
            .filter(attachments::uploaded_at.le(uploaded_before))
            .filter(diesel::dsl::not(exists(session_attachments::table.filter(
                session_attachments::attachment_id.eq(attachments::id),
            )))),
    )
    .execute(connection)?;
    Ok(())
}
