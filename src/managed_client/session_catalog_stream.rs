//! Snapshot-first Session catalog stream and reconnect reconciliation.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{
    RuntimeDescriptor, SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT,
    SessionCatalogChange, SessionCatalogRevision, SessionCatalogSnapshot, SessionCatalogUpdate,
    SessionDeleted, SessionId, SessionTitleChanged,
};

use super::ManagedEvent;

pub(super) async fn open(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
) -> reqwest::Result<reqwest::Response> {
    http.get(format!("{}/v1/session-events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await?
        .error_for_status()
}

pub(super) enum StreamOutcome {
    Disconnected,
    ReceiverClosed,
}

pub(super) async fn consume(
    response: reqwest::Response,
    events: &mpsc::Sender<ManagedEvent>,
    known_session_ids: &mut Option<HashSet<SessionId>>,
    hydrated: oneshot::Sender<()>,
) -> Result<StreamOutcome> {
    let mut stream = response.bytes_stream().eventsource();
    let mut revision = None;
    let mut hydrated = Some(hydrated);
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(event) => event,
            Err(EventStreamError::Transport(_)) => return Ok(StreamOutcome::Disconnected),
            Err(error) => bail!("Session catalog stream failed: {error}"),
        };
        match decode_event(event, revision)? {
            CatalogEvent::Snapshot(snapshot) => {
                let reconnect_snapshot = known_session_ids.is_some().then(|| snapshot.clone());
                let snapshot_revision = snapshot.revision;
                let deleted = reconcile_snapshot(known_session_ids, snapshot);
                revision = Some(snapshot_revision);
                for session_id in deleted {
                    if events
                        .send(ManagedEvent::SessionDeleted(SessionDeleted { session_id }))
                        .await
                        .is_err()
                    {
                        return Ok(StreamOutcome::ReceiverClosed);
                    }
                }
                if let Some(snapshot) = reconnect_snapshot
                    && events
                        .send(ManagedEvent::SessionCatalogReconciled(snapshot))
                        .await
                        .is_err()
                {
                    return Ok(StreamOutcome::ReceiverClosed);
                }
                if let Some(hydrated) = hydrated.take() {
                    let _ = hydrated.send(());
                }
            }
            CatalogEvent::Update(update) => {
                let announced = apply_update(known_session_ids, update.change)?;
                revision = Some(update.revision);
                if let Some(announced) = announced
                    && events.send(announced).await.is_err()
                {
                    return Ok(StreamOutcome::ReceiverClosed);
                }
            }
        }
    }
    Ok(StreamOutcome::Disconnected)
}

fn reconcile_snapshot(
    known_session_ids: &mut Option<HashSet<SessionId>>,
    snapshot: SessionCatalogSnapshot,
) -> Vec<SessionId> {
    let next_ids = snapshot.session_ids.into_iter().collect::<HashSet<_>>();
    let mut deleted = known_session_ids
        .as_ref()
        .map(|known| known.difference(&next_ids).copied().collect::<Vec<_>>())
        .unwrap_or_default();
    deleted.sort_unstable_by_key(ToString::to_string);
    *known_session_ids = Some(next_ids);
    deleted
}

/// Folds one catalog change into what this client knows the catalog holds, and
/// answers with the event a reader should hear about — `None` for a change that
/// only moves the client's own bookkeeping.
fn apply_update(
    known_session_ids: &mut Option<HashSet<SessionId>>,
    change: SessionCatalogChange,
) -> Result<Option<ManagedEvent>> {
    let known = known_session_ids
        .as_mut()
        .expect("a catalog update is decoded only after its snapshot");
    match change {
        SessionCatalogChange::Created { session_id } => {
            if !known.insert(session_id) {
                bail!("Session catalog created an existing Session");
            }
            Ok(None)
        }
        SessionCatalogChange::Deleted { session_id } => {
            if !known.remove(&session_id) {
                bail!("Session catalog deleted an unknown Session");
            }
            Ok(Some(ManagedEvent::SessionDeleted(SessionDeleted {
                session_id,
            })))
        }
        SessionCatalogChange::TitleChanged {
            session_id,
            title,
            emoji,
        } => {
            if !known.contains(&session_id) {
                bail!("Session catalog retitled an unknown Session");
            }
            Ok(Some(ManagedEvent::SessionTitleChanged(
                SessionTitleChanged {
                    session_id,
                    title,
                    emoji,
                },
            )))
        }
    }
}

enum CatalogEvent {
    Snapshot(SessionCatalogSnapshot),
    Update(SessionCatalogUpdate),
}

fn decode_event(event: Event, revision: Option<SessionCatalogRevision>) -> Result<CatalogEvent> {
    match event.event.as_str() {
        SESSION_CATALOG_SNAPSHOT_EVENT if revision.is_none() => {
            let snapshot: SessionCatalogSnapshot =
                serde_json::from_str(&event.data).context("decode Session catalog snapshot")?;
            validate_event_id(&event.id, snapshot.revision)?;
            Ok(CatalogEvent::Snapshot(snapshot))
        }
        SESSION_CATALOG_SNAPSHOT_EVENT => bail!("Session catalog sent more than one snapshot"),
        SESSION_CATALOG_UPDATED_EVENT => {
            let Some(previous) = revision else {
                bail!("Session catalog updated before its snapshot");
            };
            let update: SessionCatalogUpdate =
                serde_json::from_str(&event.data).context("decode Session catalog update")?;
            validate_event_id(&event.id, update.revision)?;
            if !update.revision.immediately_follows(previous) {
                bail!("Session catalog revision sequence is discontinuous");
            }
            Ok(CatalogEvent::Update(update))
        }
        name => bail!("server sent unknown Session catalog event type '{name}'"),
    }
}

fn validate_event_id(id: &str, revision: SessionCatalogRevision) -> Result<()> {
    if id == revision.0.to_string() {
        Ok(())
    } else {
        bail!("Session catalog event id does not match its revision")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_snapshot_recovers_a_missed_deletion() {
        let retained = SessionId::new();
        let deleted = SessionId::new();
        let created_after_connect = SessionId::new();
        let mut known = None;

        assert!(
            reconcile_snapshot(
                &mut known,
                SessionCatalogSnapshot {
                    revision: SessionCatalogRevision::INITIAL,
                    session_ids: vec![retained, deleted],
                },
            )
            .is_empty(),
            "initial hydration does not report historical deletions"
        );
        assert_eq!(
            apply_update(
                &mut known,
                SessionCatalogChange::Created {
                    session_id: created_after_connect,
                },
            )
            .expect("track the created Session"),
            None
        );

        assert_eq!(
            reconcile_snapshot(
                &mut known,
                SessionCatalogSnapshot {
                    revision: SessionCatalogRevision(4),
                    session_ids: vec![retained, created_after_connect],
                },
            ),
            vec![deleted],
            "reconnect reconciliation reports the deletion missed while disconnected"
        );
    }
}
