//! One bounded observation loop per Server. Catalog subscriptions express
//! interest; Sessions sharing a checkout consume one reading per poll.
use super::SessionStore;
use crate::{protocol::*, source_control::SourceControlService};
use futures_util::{StreamExt, stream};
use std::{collections::HashMap, time::Duration};

impl SessionStore {
    pub(crate) fn checkout_references(&self, id: &CheckoutId) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        let mut affected = 0;
        let mut working = 0;
        for record in state.sessions.values() {
            let session = &record.summary.session;
            if session
                .checkout
                .as_ref()
                .is_some_and(|checkout| &checkout.id == id)
            {
                affected += 1;
                working += usize::from(session.working_since.is_some());
            }
        }
        (affected, working)
    }

    pub(crate) fn observe_checkouts(
        &self,
        source_control: SourceControlService,
        interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let sessions = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    _ = ticker.tick() => {}
                }
                let checkouts = {
                    let mut state = sessions.state.lock().unwrap();
                    if !state.catalog.has_subscribers() {
                        // A later subscriber must not mistake an idle cached
                        // reading for an observation made during its interest.
                        for record in state.sessions.values_mut() {
                            record.summary.checkout_state = None;
                        }
                        continue;
                    }
                    state
                        .sessions
                        .values()
                        .filter(|record| record.summary.session.parent.is_none())
                        .filter_map(|record| record.summary.session.checkout.as_ref())
                        .map(|checkout| (checkout.id.clone(), checkout.clone()))
                        .collect::<HashMap<_, _>>()
                };
                let readings = stream::iter(checkouts.into_values().map(|checkout| {
                    let source_control = &source_control;
                    async move { source_control.observe(&checkout).await }
                }))
                .buffer_unordered(8);
                tokio::pin!(readings);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        reading = readings.next() => {
                            let Some(reading) = reading else { break };
                            if let Err(error) = sessions.record_checkout(reading) {
                                tracing::warn!(%error, "Checkout recovery facts could not be persisted");
                            }
                        }
                    }
                }
            }
        });
    }

    pub(crate) fn record_checkout(&self, mut reading: CheckoutSummary) -> anyhow::Result<()> {
        // Recovery fields are never part of the live reading.
        reading.association.recovery_revision = None;
        let mut state = self.state.lock().unwrap();
        let observed = state.catalog.has_subscribers();
        let mut checkout_changed = false;
        let mut invalidated = Vec::new();
        for record in state.sessions.values_mut() {
            let Some(checkout) = record.summary.session.checkout.as_ref() else {
                continue;
            };
            if checkout.id != reading.association.id {
                continue;
            }
            let recovery_changed = reading.availability == SourceControlAvailability::Available
                && checkout.recovery_revision != reading.revision;
            if recovery_changed {
                let mut checkout = checkout.clone();
                checkout.recovery_revision = reading.revision.clone();
                let update = SessionUpdate {
                    session_id: record.summary.session.id,
                    revision: SessionRevision(
                        record
                            .snapshot
                            .revision
                            .0
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("Session revision exhausted"))?,
                    ),
                    changes: vec![SessionChange::WorkspaceChanged {
                        workspace: record.summary.session.workspace.clone(),
                        checkout: Some(checkout),
                    }],
                };
                // Only the current association is changed, under the same lock
                // used by regrouping. A completed read cannot overwrite a more
                // recently resolved Workspace or checkout association.
                let mut next = record.snapshot.session.clone();
                if let SessionChange::WorkspaceChanged { checkout, .. } = &update.changes[0] {
                    next.checkout = checkout.clone();
                }
                self.storage.location_changed(next, update.revision)?;
                crate::session_projection::apply_update(&mut record.snapshot, &update)?;
                record.summary.session = record.snapshot.session.clone();
                let _ = record.updates.send(update);
            }
            let live = observed.then(|| reading.clone());
            if record.summary.checkout_state != live {
                record.summary.checkout_state = live.clone();
                checkout_changed = true;
            }
            if recovery_changed && record.summary.session.parent.is_none() {
                invalidated.push(record.summary.session.id);
            }
        }
        for session_id in invalidated {
            state.publish_catalog_change(SessionCatalogChange::Invalidated { session_id });
        }
        if checkout_changed {
            state.publish_catalog_change(SessionCatalogChange::CheckoutStateChanged {
                checkout_id: reading.association.id.clone(),
                checkout_state: observed.then_some(reading),
            });
        }
        Ok(())
    }
}
