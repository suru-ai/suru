//! One bounded observation loop per Server. Catalog subscriptions express
//! interest; every Worktree of every Repository this Server knows is then read
//! once per poll, and Sessions sharing a checkout share that one reading.
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

    /// Watch every Worktree of every Repository this Server knows, for as long
    /// as some catalog subscriber is interested.
    ///
    /// The observed set is not the Sessions' checkouts: it is every Worktree of
    /// every Repository a Workspace has been grouped for, whether that grouping
    /// came from a Session at startup or from a Workspace resolution since,
    /// together with the checkouts Sessions themselves name. A Worktree no
    /// Session works in is watched exactly like one that several share.
    ///
    /// Each tick re-enumerates, so a Worktree added or removed outside Suru is
    /// picked up, and the work stays bounded: one listing per Repository, then
    /// one reading per distinct Worktree.
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
                let session_checkouts = {
                    let mut state = sessions.state.lock().unwrap();
                    if !state.catalog.has_subscribers() {
                        // A later subscriber must not mistake an idle cached
                        // reading for an observation made during its interest.
                        for record in state.sessions.values_mut() {
                            record.summary.checkout_state = None;
                        }
                        state.observed_checkouts.clear();
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
                let listings =
                    stream::iter(source_control.repositories().into_iter().map(|repository| {
                        let source_control = &source_control;
                        async move {
                            let listed = source_control.list_checkouts(&repository).await;
                            (repository.id, listed)
                        }
                    }))
                    .buffer_unordered(4);
                let mut checkouts = session_checkouts;
                if drain_until_shutdown(listings, &mut shutdown, |(repository, listed)| {
                    // A listing that could not be taken says nothing about which
                    // Worktrees this Repository has, so the ones last observed
                    // stand for this tick rather than being retired now and
                    // announced again the moment the listing answers.
                    let associations = match listed {
                        Ok(listed) => listed,
                        Err(error) => {
                            tracing::debug!(%error, "Repository Worktree listing failed");
                            sessions.observed_checkouts_of(&repository)
                        }
                    };
                    for association in associations {
                        // A Session's own association carries the recovery facts
                        // a listing cannot name.
                        checkouts
                            .entry(association.id.clone())
                            .or_insert(association);
                    }
                })
                .await
                .is_break()
                {
                    return;
                }
                sessions.retire_unobserved_checkouts(&checkouts);
                let readings = stream::iter(checkouts.into_values().map(|checkout| {
                    let source_control = &source_control;
                    async move { source_control.observe(&checkout).await }
                }))
                .buffer_unordered(8);
                if drain_until_shutdown(readings, &mut shutdown, |reading| {
                    if let Err(error) = sessions.record_checkout(reading) {
                        tracing::warn!(%error, "Checkout recovery facts could not be persisted");
                    }
                })
                .await
                .is_break()
                {
                    return;
                }
            }
        });
    }

    /// The Worktrees this Server last observed for one Repository, as the
    /// associations a fresh listing would have named them by. A tick whose
    /// listing failed stands on these, so a Repository that could not answer
    /// keeps the Worktrees it had rather than losing and regaining them.
    fn observed_checkouts_of(&self, repository: &RepositoryId) -> Vec<CheckoutAssociation> {
        self.state
            .lock()
            .unwrap()
            .observed_checkouts
            .values()
            .filter(|reading| &reading.association.repository == repository)
            .map(|reading| reading.association.clone())
            .collect()
    }

    /// Let go of the Worktrees this tick no longer observes, saying so once on
    /// the catalog exactly as an ended interest does.
    fn retire_unobserved_checkouts(&self, observed: &HashMap<CheckoutId, CheckoutAssociation>) {
        let mut state = self.state.lock().unwrap();
        let retired = state
            .observed_checkouts
            .keys()
            .filter(|id| !observed.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for checkout_id in retired {
            state.observed_checkouts.remove(&checkout_id);
            for record in state.sessions.values_mut() {
                if record
                    .summary
                    .session
                    .checkout
                    .as_ref()
                    .is_some_and(|checkout| checkout.id == checkout_id)
                {
                    record.summary.checkout_state = None;
                }
            }
            state.publish_catalog_change(SessionCatalogChange::CheckoutStateChanged {
                checkout_id,
                checkout_state: None,
            });
        }
    }

    pub(crate) fn record_checkout(&self, mut reading: CheckoutSummary) -> anyhow::Result<()> {
        // Recovery fields are never part of the live reading.
        reading.association.recovery_revision = None;
        let mut state = self.state.lock().unwrap();
        let observed = state.catalog.has_subscribers();
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
            record.summary.checkout_state = observed.then(|| reading.clone());
            if recovery_changed && record.summary.session.parent.is_none() {
                invalidated.push(record.summary.session.id);
            }
        }
        for session_id in invalidated {
            state.publish_catalog_change(SessionCatalogChange::Invalidated { session_id });
        }
        // The Worktree's reading is the catalog's, not any one Session's: what
        // decides whether to announce it is whether this Worktree's own last
        // reading changed, so a Worktree no Session works in announces too.
        let checkout_id = reading.association.id.clone();
        let changed = if observed {
            state
                .observed_checkouts
                .insert(checkout_id.clone(), reading.clone())
                .as_ref()
                != Some(&reading)
        } else {
            state.observed_checkouts.remove(&checkout_id).is_some()
        };
        if changed {
            state.publish_catalog_change(SessionCatalogChange::CheckoutStateChanged {
                checkout_id,
                checkout_state: observed.then_some(reading),
            });
        }
        Ok(())
    }
}

/// Hand every finished piece of work in `stream` to `handle`, and give the
/// whole observation up the moment shutdown is asked for: a tick already in
/// flight is never worth waiting out. Breaking says the loop must return
/// rather than go on to its next stage.
async fn drain_until_shutdown<S: futures_util::Stream>(
    stream: S,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    mut handle: impl FnMut(S::Item),
) -> std::ops::ControlFlow<()> {
    tokio::pin!(stream);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return std::ops::ControlFlow::Break(()),
            item = stream.next() => {
                let Some(item) = item else {
                    return std::ops::ControlFlow::Continue(());
                };
                handle(item);
            }
        }
    }
}
