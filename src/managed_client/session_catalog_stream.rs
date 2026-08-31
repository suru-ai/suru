//! Snapshot-first Session catalog stream and reconnect reconciliation.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::Duration,
};

use crate::protocol::{
    Outlook, RuntimeDescriptor, SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT,
    SKILL_CATALOG_UPDATED_EVENT, SessionCatalogChange, SessionCatalogRevision,
    SessionCatalogSnapshot, SessionCatalogUpdate, SessionCreated, SessionDeleted, SessionId,
    SessionSettlementChanged, SessionTitleChanged, SessionUsageChanged, SessionWorkingChanged,
    SkillCatalog,
};

use super::{ManagedEvent, server_url};

pub(super) async fn open(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
) -> reqwest::Result<reqwest::Response> {
    open_for_outlook(http, descriptor, &Outlook::Local).await
}

async fn open_for_outlook(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    outlook: &Outlook,
) -> reqwest::Result<reqwest::Response> {
    let url = server_url(&descriptor.base_url, outlook, "/v1/session-events")
        .expect("a validated runtime descriptor builds a Server URL");
    http.get(url)
        .bearer_auth(&descriptor.token)
        .send()
        .await?
        .error_for_status()
}

/// A reconnecting catalog stream for the Server named by one Client Outlook.
/// Its lifetime is the interest: dropping it aborts the task and releases the
/// Remote proxy connection.
pub struct SessionCatalogSubscription {
    events: mpsc::Receiver<ManagedEvent>,
    task: JoinHandle<()>,
}

impl SessionCatalogSubscription {
    pub(super) fn open_attached(
        http: reqwest::Client,
        descriptor: watch::Receiver<RuntimeDescriptor>,
        outlook: Outlook,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(run_attached(
            http,
            descriptor,
            outlook,
            events_tx,
            initial_backoff,
            max_backoff,
        ));
        Self {
            events: events_rx,
            task,
        }
    }

    pub async fn next(&mut self) -> Option<ManagedEvent> {
        self.events.recv().await
    }
}

impl Drop for SessionCatalogSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_attached(
    http: reqwest::Client,
    mut descriptor: watch::Receiver<RuntimeDescriptor>,
    outlook: Outlook,
    events: mpsc::Sender<ManagedEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    // An attached Outlook has no separately hydrated local catalog. Treat its
    // first snapshot as reconciliation so the TUI refreshes any listing that
    // raced with opening the stream.
    let mut known_session_ids = Some(HashSet::new());
    let mut retry_in = Duration::ZERO;
    loop {
        if events.is_closed() {
            return;
        }
        let active_descriptor = descriptor.borrow().clone();
        let response = open_for_outlook(&http, &active_descriptor, &outlook).await;
        let Ok(response) = response else {
            retry_in = next_backoff(retry_in, initial_backoff, max_backoff);
            if wait_to_reconnect(&mut descriptor, retry_in).await {
                retry_in = Duration::ZERO;
            }
            continue;
        };
        let (hydrated, hydration) = oneshot::channel();
        match consume(response, &events, &mut known_session_ids, hydrated).await {
            Ok(StreamOutcome::ReceiverClosed) => return,
            Ok(StreamOutcome::Disconnected) | Err(_) => {
                if hydration.await.is_ok() {
                    retry_in = Duration::ZERO;
                }
                retry_in = next_backoff(retry_in, initial_backoff, max_backoff);
                if wait_to_reconnect(&mut descriptor, retry_in).await {
                    retry_in = Duration::ZERO;
                }
            }
        }
    }
}

/// Waits for the retry delay or a replacement local Server descriptor. A new
/// descriptor starts a fresh retry schedule because it names a new proxy.
async fn wait_to_reconnect(
    descriptor: &mut watch::Receiver<RuntimeDescriptor>,
    retry_in: Duration,
) -> bool {
    tokio::select! {
        changed = descriptor.changed(), if descriptor.has_changed().is_ok() => changed.is_ok(),
        _ = tokio::time::sleep(retry_in) => false,
    }
}

fn next_backoff(previous: Duration, initial: Duration, max: Duration) -> Duration {
    if previous.is_zero() {
        initial
    } else {
        previous.saturating_mul(2).min(max)
    }
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
                if events.send(announced).await.is_err() {
                    return Ok(StreamOutcome::ReceiverClosed);
                }
            }
            CatalogEvent::Skill(catalog) => {
                if events
                    .send(ManagedEvent::SkillCatalogUpdated(catalog))
                    .await
                    .is_err()
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
/// answers with the event a reader should hear about. Every change is worth
/// hearing: a surface listing Sessions is only as truthful as the last change
/// it was told about.
fn apply_update(
    known_session_ids: &mut Option<HashSet<SessionId>>,
    change: SessionCatalogChange,
) -> Result<ManagedEvent> {
    let known = known_session_ids
        .as_mut()
        .expect("a catalog update is decoded only after its snapshot");
    match change {
        SessionCatalogChange::Created { session_id } => {
            if !known.insert(session_id) {
                bail!("Session catalog created an existing Session");
            }
            Ok(ManagedEvent::SessionCreated(SessionCreated { session_id }))
        }
        SessionCatalogChange::Deleted { session_id } => {
            if !known.remove(&session_id) {
                bail!("Session catalog deleted an unknown Session");
            }
            Ok(ManagedEvent::SessionDeleted(SessionDeleted { session_id }))
        }
        SessionCatalogChange::TitleChanged {
            session_id,
            title,
            emoji,
        } => {
            if !known.contains(&session_id) {
                bail!("Session catalog retitled an unknown Session");
            }
            Ok(ManagedEvent::SessionTitleChanged(SessionTitleChanged {
                session_id,
                title,
                emoji,
            }))
        }
        SessionCatalogChange::SettlementChanged {
            session_id,
            settled_at,
        } => {
            if !known.contains(&session_id) {
                bail!("Session catalog settled an unknown Session");
            }
            Ok(ManagedEvent::SessionSettlementChanged(
                SessionSettlementChanged {
                    session_id,
                    settled_at,
                },
            ))
        }
        SessionCatalogChange::WorkingChanged {
            session_id,
            working_since,
        } => {
            if !known.contains(&session_id) {
                bail!("Session catalog reported work on an unknown Session");
            }
            Ok(ManagedEvent::SessionWorkingChanged(SessionWorkingChanged {
                session_id,
                working_since,
            }))
        }
        SessionCatalogChange::UsageChanged {
            session_id,
            total_usage,
        } => {
            if !known.contains(&session_id) {
                bail!("Session catalog reported Usage on an unknown Session");
            }
            Ok(ManagedEvent::SessionUsageChanged(SessionUsageChanged {
                session_id,
                total_usage,
            }))
        }
    }
}

enum CatalogEvent {
    Snapshot(SessionCatalogSnapshot),
    Update(SessionCatalogUpdate),
    Skill(SkillCatalog),
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
        SKILL_CATALOG_UPDATED_EVENT => {
            let catalog: SkillCatalog =
                serde_json::from_str(&event.data).context("decode Skill Catalog update")?;
            Ok(CatalogEvent::Skill(catalog))
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
    use crate::managed_client::{INITIAL_RECOVERY_BACKOFF, MAX_RECOVERY_BACKOFF};
    use crate::protocol::SessionTimestamp;

    #[test]
    fn attached_catalog_backoff_doubles_and_caps_at_the_managed_client_limit() {
        let mut retry_in = Duration::ZERO;
        let waits = (0..10)
            .map(|_| {
                retry_in = next_backoff(retry_in, INITIAL_RECOVERY_BACKOFF, MAX_RECOVERY_BACKOFF);
                retry_in
            })
            .collect::<Vec<_>>();

        assert_eq!(
            waits,
            [50, 100, 200, 400, 800, 1600, 3200, 5000, 5000, 5000].map(Duration::from_millis)
        );
    }

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
            ManagedEvent::SessionCreated(SessionCreated {
                session_id: created_after_connect,
            }),
            "a Session another client made is announced to every client listing Sessions"
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

    #[test]
    fn a_working_change_is_announced_for_a_listed_session_and_refused_for_an_unknown_one() {
        let listed = SessionId::new();
        let mut known = None;
        reconcile_snapshot(
            &mut known,
            SessionCatalogSnapshot {
                revision: SessionCatalogRevision::INITIAL,
                session_ids: vec![listed],
            },
        );

        assert_eq!(
            apply_update(
                &mut known,
                SessionCatalogChange::WorkingChanged {
                    session_id: listed,
                    working_since: Some(SessionTimestamp(7)),
                },
            )
            .expect("announce the Turn another client's Session is running"),
            ManagedEvent::SessionWorkingChanged(SessionWorkingChanged {
                session_id: listed,
                working_since: Some(SessionTimestamp(7)),
            }),
            "a Turn starting in a Session this client never opened is \
             announced to every surface listing it"
        );
        assert!(
            apply_update(
                &mut known,
                SessionCatalogChange::WorkingChanged {
                    session_id: SessionId::new(),
                    working_since: None,
                },
            )
            .is_err(),
            "work reported on a Session the catalog never listed is a \
             discontinuity, not something to draw"
        );
    }
}
