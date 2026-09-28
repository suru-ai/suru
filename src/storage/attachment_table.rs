//! Attachment rows: written when their bytes are first uploaded, stamped
//! referenced again whenever the same bytes are uploaded again or a Prompt
//! binding them is admitted, joined to the Sessions whose stored Prompts and
//! Messages bind them, and described without their bytes to the Sessions that
//! bind them. The bytes are read back only when something asks for them (ADR
//! 0037). An Attachment's age, which decides whether it may be reclaimed, is
//! measured from that last reference.
//!
//! One no Session is joined to — bound by no stored Prompt or Message — is
//! swept once that age passes the grace period. The Server sweeps once at
//! start, after loading its Sessions; then at the idle flush ending each
//! burst of the storage writer's work, once every Session it holds has
//! landed its joins; and, since an upload alone never wakes the writer, at
//! an idle tick whenever the sweep interval has passed since the last sweep
//! of any kind, so a quiet Server still reclaims a paste whose label was
//! deleted.

use std::{
    collections::{BTreeSet, HashSet},
    sync::atomic::Ordering,
};

use diesel::{
    SqliteConnection,
    dsl::{exists, not},
    prelude::*,
    query_builder::BoxedDeleteStatement,
    sqlite::Sqlite,
};

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
    referenced_at: i64,
    bytes: Vec<u8>,
}

/// A moment as the `referenced_at` column stores it.
fn millis(moment: SessionTimestamp) -> i64 {
    i64::try_from(moment.0).unwrap_or(i64::MAX)
}

impl StorageRepository {
    /// Stores an upload's bytes under its descriptor, or where the same bytes
    /// already are, stamps them referenced now; answers whether this call
    /// stored them.
    pub(crate) async fn store_attachment(
        &self,
        descriptor: AttachmentDescriptor,
        bytes: Vec<u8>,
    ) -> Result<bool, StorageError> {
        let path = self.database_path.clone();
        let now = millis(self.clock.now());
        on_blocking_task("store Attachment", move || {
            let AttachmentKind::Image { width, height } = descriptor.kind;
            let row = AttachmentRow {
                id: descriptor.id.as_str().to_owned(),
                mime_type: descriptor.mime_type,
                byte_length: i64::try_from(descriptor.byte_length)
                    .map_err(|error| StorageError::WriteAttachment(error.to_string()))?,
                width: Some(width.into()),
                height: Some(height.into()),
                referenced_at: now,
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
                            .set(attachments::referenced_at.eq(row.referenced_at))
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

    /// Stamps every one of the named Attachments that is stored as
    /// referenced now, in one statement, and answers what each of them that
    /// is stored was described as when it was uploaded, in id order. Once
    /// stamped, an Attachment stays within its grace period for as long as it
    /// takes the Prompt binding it to be recorded and joined.
    pub(crate) async fn reference_attachments(
        &self,
        ids: Vec<AttachmentId>,
    ) -> Result<Vec<AttachmentDescriptor>, StorageError> {
        let path = self.database_path.clone();
        let now = millis(self.clock.now());
        on_blocking_task("reference Attachments", move || {
            let named = ids
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect::<BTreeSet<_>>();
            let mut connection = super::connect(&path)?;
            diesel::update(attachments::table.filter(attachments::id.eq_any(&named)))
                .set(attachments::referenced_at.eq(now))
                .execute(&mut connection)
                .map_err(|error| StorageError::WriteAttachment(error.to_string()))?;
            stored_descriptors(&mut connection, &named)
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

    /// Sweeps orphaned Attachments: see [`sweep_orphaned_attachments`].
    pub(crate) async fn sweep_orphaned_attachments(&self) -> Result<usize, StorageError> {
        let repository = self.clone();
        on_blocking_task("sweep Attachments", move || {
            sweep_orphaned_attachments(&repository)
        })
        .await
    }

    /// The latest an Attachment may have last been referenced and still be
    /// reclaimed now: the grace period ago.
    pub(super) fn grace_cutoff(&self) -> i64 {
        millis(self.clock.now())
            .saturating_sub(i64::try_from(self.attachment_grace.as_millis()).unwrap_or(i64::MAX))
    }

    /// Whether the sweep interval has passed since orphaned Attachments were
    /// last swept, or the clock has been set back behind that sweep.
    pub(super) fn attachment_sweep_due(&self) -> bool {
        let since = millis(self.clock.now())
            .saturating_sub(self.attachments_swept_at.load(Ordering::SeqCst));
        let interval =
            i64::try_from(self.attachment_sweep_interval.as_millis()).unwrap_or(i64::MAX);
        !(0..interval).contains(&since)
    }
}

/// Deletes every Attachment no stored Prompt or Message binds — one no
/// Session is joined to — that was last uploaded or bound no later than the
/// grace period ago, and answers how many it deleted. Run where every Session
/// held in memory has landed its joins, it leaves only an upload whose Prompt
/// is still in admission unjoined, and admission stamped that one referenced
/// within the grace period. A sweep that fails still counts as the last one,
/// so a failing database is not retried at every idle tick.
pub(super) fn sweep_orphaned_attachments(
    repository: &StorageRepository,
) -> Result<usize, StorageError> {
    repository
        .attachments_swept_at
        .store(millis(repository.clock.now()), Ordering::SeqCst);
    let referenced_before = repository.grace_cutoff();
    let mut connection = super::connect(&repository.database_path)?;
    let swept = delete_unjoined(referenced_before)
        .execute(&mut connection)
        .map_err(|error| StorageError::WriteAttachment(error.to_string()))?;
    if swept > 0 {
        tracing::info!(swept, "swept Attachments no stored Prompt or Message binds");
    }
    Ok(swept)
}

/// The descriptors the named Attachments that are stored were uploaded as, in
/// id order, read without their bytes. A row whose kind this build cannot
/// describe is reported to the Log and passed over.
pub(super) fn stored_descriptors<'a>(
    connection: &mut SqliteConnection,
    ids: impl IntoIterator<Item = &'a String>,
) -> Result<Vec<AttachmentDescriptor>, diesel::result::Error> {
    let rows = attachments::table
        .filter(attachments::id.eq_any(ids))
        .order(attachments::id.asc())
        .select((
            attachments::id,
            attachments::mime_type,
            attachments::byte_length,
            attachments::width,
            attachments::height,
        ))
        .load::<(String, String, i64, Option<i64>, Option<i64>)>(connection)?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, mime_type, byte_length, width, height)| {
            let dimensions = width.zip(height).and_then(|(width, height)| {
                Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
            });
            let (Some((width, height)), Ok(byte_length)) = (dimensions, u64::try_from(byte_length))
            else {
                tracing::warn!(
                    attachment_id = id,
                    "a stored Attachment cannot be described"
                );
                return None;
            };
            Some(AttachmentDescriptor {
                id: AttachmentId::new(id),
                kind: AttachmentKind::Image { width, height },
                mime_type,
                byte_length,
            })
        })
        .collect())
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
/// longer and that were last referenced no later than `referenced_before`. An
/// upload no Session has bound yet is never among them.
pub(super) fn delete_unjoined_attachments(
    connection: &mut SqliteConnection,
    attachment_ids: &[String],
    referenced_before: i64,
) -> Result<(), diesel::result::Error> {
    if attachment_ids.is_empty() {
        return Ok(());
    }
    delete_unjoined(referenced_before)
        .filter(attachments::id.eq_any(attachment_ids))
        .execute(connection)?;
    Ok(())
}

/// The deletion of every Attachment no Session is joined to that was last
/// referenced no later than `referenced_before`.
fn delete_unjoined(
    referenced_before: i64,
) -> BoxedDeleteStatement<'static, Sqlite, attachments::table> {
    diesel::delete(attachments::table)
        .filter(attachments::referenced_at.le(referenced_before))
        .filter(not(exists(session_attachments::table.filter(
            session_attachments::attachment_id.eq(attachments::id),
        ))))
        .into_boxed()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::storage::{connect, sessions};

    #[tokio::test]
    async fn referencing_attachments_describes_those_stored() {
        let directory = tempfile::tempdir().expect("create data directory");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open repository");
        let bytes = b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec();
        let descriptor = crate::attachments::describe(&bytes).expect("describe fixture");
        assert!(
            repository
                .store_attachment(descriptor.clone(), bytes)
                .await
                .expect("store fixture")
        );
        let missing = AttachmentId::new("never-uploaded");

        let stored = repository
            .reference_attachments(vec![descriptor.id.clone(), missing.clone()])
            .await
            .expect("reference Attachments");
        assert_eq!(
            stored,
            vec![descriptor.clone()],
            "a stored id is described as it was uploaded, and a missing one not at all"
        );

        let all = repository
            .reference_attachments(vec![descriptor.id.clone(), descriptor.id.clone()])
            .await
            .expect("reference one Attachment twice");
        assert_eq!(all, vec![descriptor]);
    }

    async fn stored(repository: &StorageRepository, width: u8) -> AttachmentId {
        let bytes = [b"GIF89a".as_slice(), &[width, 0, 1, 0, 0, 0, 0, 0]].concat();
        let descriptor = crate::attachments::describe(&bytes).expect("describe fixture");
        let id = descriptor.id.clone();
        repository
            .store_attachment(descriptor, bytes)
            .await
            .expect("store fixture");
        id
    }

    #[tokio::test]
    async fn sweeping_deletes_only_attachments_no_session_joins_once_past_their_grace() {
        const MINUTE: Duration = Duration::from_secs(60);
        const HOUR: Duration = Duration::from_secs(60 * 60);
        let directory = tempfile::tempdir().expect("create data directory");
        let (clock, hand) = crate::clock::ServerClock::manual();
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open repository")
            .with_attachment_grace(HOUR)
            .with_clock(clock);
        let orphan = stored(&repository, 1).await;
        let joined = stored(&repository, 2).await;
        hand.advance(2 * MINUTE);
        let young = stored(&repository, 3).await;
        let mut connection = connect(&repository.database_path).expect("connect");
        diesel::insert_into(sessions::table)
            .values((
                sessions::id.eq("joining-session"),
                sessions::title.eq("fixture"),
                sessions::created_at.eq(0),
                sessions::updated_at.eq(0),
                sessions::workspace.eq("{}"),
                sessions::agent_selection_availability.eq("unavailable"),
                sessions::status.eq("idle"),
                sessions::revision.eq(0),
                sessions::brokered.eq(false),
            ))
            .execute(&mut connection)
            .expect("store a Session to join");
        join_session_attachments(
            &mut connection,
            "joining-session",
            &[joined.as_str().to_owned()],
        )
        .expect("join the Session to an Attachment");
        let all = || vec![orphan.clone(), joined.clone(), young.clone()];

        hand.advance(HOUR - MINUTE);
        assert_eq!(
            repository
                .sweep_orphaned_attachments()
                .await
                .expect("sweep"),
            1,
            "only the unjoined Attachment past its grace period is swept"
        );
        assert_eq!(
            repository
                .stored_attachments(all())
                .await
                .expect("find Attachments"),
            HashSet::from([joined.clone(), young.clone()])
        );

        hand.advance(2 * MINUTE);
        assert_eq!(
            repository
                .sweep_orphaned_attachments()
                .await
                .expect("sweep again"),
            1,
            "the younger unjoined Attachment goes once its grace period passes"
        );
        assert_eq!(
            repository
                .stored_attachments(all())
                .await
                .expect("find Attachments"),
            HashSet::from([joined.clone()]),
            "a joined Attachment stays however old"
        );
        assert_eq!(
            repository
                .sweep_orphaned_attachments()
                .await
                .expect("sweep with nothing to reclaim"),
            0
        );
    }

    #[tokio::test]
    async fn a_sweep_is_due_once_the_interval_passes_since_the_last_one() {
        let interval = Duration::from_secs(5 * 60);
        let directory = tempfile::tempdir().expect("create data directory");
        let (clock, hand) = crate::clock::ServerClock::manual();
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open repository")
            .with_attachment_sweep_interval(interval)
            .with_clock(clock);
        assert!(
            repository.attachment_sweep_due(),
            "a repository never swept is due a sweep"
        );

        repository
            .sweep_orphaned_attachments()
            .await
            .expect("sweep");
        assert!(!repository.attachment_sweep_due());
        hand.advance(interval - Duration::from_millis(1));
        assert!(!repository.attachment_sweep_due());
        hand.advance(Duration::from_millis(1));
        assert!(
            repository.clone().attachment_sweep_due(),
            "every handle on the repository shares when it was last swept"
        );
    }
}
