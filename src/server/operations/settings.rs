//! Changing a Setting: the one operation through which both the settings
//! panel's change and a Sidekick's `set_setting` edit the Config Document and
//! put what it then says in force, so a Setting a Sidekick changed takes
//! effect and reaches every Client exactly as one the user changed does.
//!
//! A change is whole and one at a time. It runs on a task of the Server's own,
//! so one whose caller stops waiting — a Sidekick's harness hanging up, a
//! Client gone — is still put in force rather than left written to the
//! document while the running Server and its Clients go on without it. And
//! each change holds every later one off from its write through its
//! adoption, its broadcast and the Approval Postures it reconciles, so the
//! settings in force are always those the document last said, never an older
//! change adopted after a newer one.
//!
//! The operation changes whatever Setting it is given. What a Sidekick may
//! change is bounded where its Tool is served, over the schema (ADR 0043),
//! since the user's own panel may change every Setting there is.

use std::{future::Future, sync::Arc};

use tokio::sync::{Mutex, watch};

use super::SessionOperations;
use crate::{
    protocol::{SettingMutation, SettingsSnapshot},
    provider::{ProviderOrchestrator, ProviderRuntime},
    serving::ServingController,
    sessions::SessionStore,
    settings::{ConfigDocuments, SettingsMutationError},
};

/// The Config Documents this Server alone writes, and everything that runs
/// under the Settings they leave in force and so adopts each change of them.
#[derive(Clone)]
pub(crate) struct SettingsAdoption {
    documents: ConfigDocuments,
    /// The settings in force, pushed to every attached Client as they change.
    snapshot: Arc<watch::Sender<SettingsSnapshot>>,
    /// Every hosted Provider runtime, handed the Server Settings it now runs
    /// under.
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    /// Moved to whatever the Serving Settings now say.
    serving: ServingController,
    /// Where each change runs, whole and after the one before it.
    changes: OneChangeAtATime,
}

impl SettingsAdoption {
    pub(in crate::server) fn new(
        documents: ConfigDocuments,
        snapshot: Arc<watch::Sender<SettingsSnapshot>>,
        runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
        serving: ServingController,
    ) -> Self {
        Self {
            documents,
            snapshot,
            runtimes,
            serving,
            changes: OneChangeAtATime::default(),
        }
    }
}

/// A change of a Setting that landed: written to the Config Document and in
/// force everywhere it could be put.
pub(crate) struct SettingChanged {
    /// The settings in force now, as every attached Client was told them.
    pub(crate) snapshot: SettingsSnapshot,
    /// Why the Serving listener could not be moved to what the Serving
    /// Settings now say, where it could not. Everything else follows them all
    /// the same, the Approval Postures they make included.
    pub(crate) serving: Option<ServingNotAdopted>,
}

/// Why the Serving listener could not follow the Serving Settings. What went
/// wrong names the address and port it could not take — the values of
/// Settings no Sidekick may read — so it has no `Display` and no `Debug` that
/// shows it: only the user's own surfaces and the Log read [`Self::detail`],
/// and anything answering a Sidekick words the failure itself.
pub(crate) struct ServingNotAdopted(anyhow::Error);

impl ServingNotAdopted {
    /// What went wrong, for the user's own surfaces alone.
    pub(crate) fn detail(&self) -> String {
        self.0.to_string()
    }

    /// The listener's failure as `error` says it, for a test standing in for
    /// one.
    #[cfg(test)]
    pub(crate) fn because(error: anyhow::Error) -> Self {
        Self(error)
    }
}

impl std::fmt::Debug for ServingNotAdopted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ServingNotAdopted(..)")
    }
}

impl SessionOperations {
    /// Applies `mutation` to the winning Config Document and puts the
    /// settings the reloaded document yields in force: every hosted Provider
    /// runtime takes the Server Settings it now runs under, the Serving
    /// listener moves to what its Settings say, every attached Client is
    /// pushed the snapshot, and each live Session following a Provider's
    /// Approval Posture Settings takes the posture they now make. The change
    /// runs whole, after every change asked for before it has, and on a task
    /// of the Server's own, so it completes whether or not this is awaited to
    /// the end. A document that cannot take the edit changes nothing.
    pub(crate) async fn change_setting(
        &self,
        mutation: SettingMutation,
    ) -> Result<SettingChanged, SettingsMutationError> {
        let adoption = self.settings_adoption.clone();
        let sessions = self.sessions.clone();
        let providers = self.providers.clone();
        self.settings_adoption
            .changes
            .run(async move { adoption.change(mutation, &sessions, &providers).await })
            .await
    }
}

impl SettingsAdoption {
    /// One change, from its write through the Approval Postures it reconciles.
    async fn change(
        &self,
        mutation: SettingMutation,
        sessions: &SessionStore,
        providers: &ProviderOrchestrator,
    ) -> Result<SettingChanged, SettingsMutationError> {
        // The edit is filesystem work, and the CST handles it parses the
        // document into are not `Send`; both stay on a blocking thread, where
        // the read, the edit, and the write are one scope.
        let documents = self.documents.clone();
        let snapshot = tokio::task::spawn_blocking(move || documents.mutate(&mutation))
            .await
            .expect("Config Document edit runs to completion")
            .inspect_err(|error| {
                if let SettingsMutationError::Io { .. } = error {
                    tracing::error!("Setting mutation failed: {error}");
                }
            })?;
        // The snapshot is in force, and every Client told of it, whether or
        // not the Serving listener could follow it, so the postures it makes
        // are reconciled either way.
        let serving =
            crate::server::adopt_settings(&self.snapshot, &self.runtimes, &self.serving, &snapshot)
                .await
                .err()
                .map(|error| {
                    tracing::error!("could not adopt Serving settings: {error:#}");
                    ServingNotAdopted(error)
                });
        let changed = sessions.reconcile_approval_postures(&snapshot.settings);
        crate::server::apply_live_posture_updates(providers, changed).await;
        Ok(SettingChanged { snapshot, serving })
    }
}

/// Runs changes of the Settings whole and one at a time, each on a task of the
/// Server's own.
#[derive(Clone, Default)]
struct OneChangeAtATime(Arc<Mutex<()>>);

impl OneChangeAtATime {
    /// Starts `change` at once on a task of its own, which runs it once no
    /// other change is running — every later change waiting until it has
    /// finished — and answers with its outcome. The change runs to the end
    /// whether or not the answer is awaited.
    fn run<T, F>(&self, change: F) -> impl Future<Output = T> + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let one_at_a_time = self.0.clone();
        let task = tokio::spawn(async move {
            let _running = one_at_a_time.lock().await;
            change.await
        });
        async move {
            task.await
                .expect("a change of the Settings runs to completion")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex as Log, time::Duration};

    use tokio::sync::oneshot;

    use super::*;

    /// `awaited`, failing the test rather than hanging it where a change never
    /// gets as far as it should.
    async fn within<T>(awaited: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), awaited)
            .await
            .expect("the change gets this far")
    }

    /// A change that says when it starts, then waits to be released, and
    /// writes down when it begins and ends.
    fn held_change(
        name: &'static str,
        log: &Arc<Log<Vec<String>>>,
    ) -> (
        impl Future<Output = &'static str> + Send + 'static,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    ) {
        let (started, started_rx) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let log = log.clone();
        let change = async move {
            log.lock().expect("log").push(format!("{name} begins"));
            started.send(()).ok();
            released.await.ok();
            log.lock().expect("log").push(format!("{name} ends"));
            name
        };
        (change, started_rx, release)
    }

    /// A caller that stops waiting partway through its change — a harness
    /// hanging up — leaves the change to run to the end, rather than leaving
    /// it written but never put in force.
    #[tokio::test]
    async fn a_change_runs_to_the_end_though_its_caller_stops_waiting() {
        let log = Arc::new(Log::new(Vec::new()));
        let changes = OneChangeAtATime::default();
        let (change, started, release) = held_change("abandoned", &log);
        let answer = changes.run(change);
        within(started)
            .await
            .expect("the change starts though no one awaits it");
        drop(answer);
        release.send(()).expect("the change is still running");

        let (after, after_started, after_release) = held_change("after", &log);
        let after = changes.run(after);
        within(after_started).await.expect("a later change starts");
        after_release.send(()).expect("release the later change");
        assert_eq!(within(after).await, "after");
        assert_eq!(
            *log.lock().expect("log"),
            [
                "abandoned begins",
                "abandoned ends",
                "after begins",
                "after ends"
            ],
            "the abandoned change ran whole before the next began"
        );
    }

    /// A change asked for while another is running waits until that one has
    /// finished whole, so no change is adopted out of the order it was
    /// written in.
    #[tokio::test]
    async fn a_change_waits_until_the_one_running_has_finished_whole() {
        let log = Arc::new(Log::new(Vec::new()));
        let changes = OneChangeAtATime::default();
        let (first, first_started, first_release) = held_change("first", &log);
        let first = changes.run(first);
        within(first_started)
            .await
            .expect("the first change starts");
        let (second, mut second_started, second_release) = held_change("second", &log);
        let second = changes.run(second);
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            second_started.try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
            "the second change waits while the first runs"
        );
        assert_eq!(*log.lock().expect("log"), ["first begins"]);

        first_release.send(()).expect("release the first change");
        assert_eq!(within(first).await, "first");
        within(second_started)
            .await
            .expect("the second change starts once the first ends");
        second_release.send(()).expect("release the second change");
        assert_eq!(within(second).await, "second");
        assert_eq!(
            *log.lock().expect("log"),
            ["first begins", "first ends", "second begins", "second ends"]
        );
    }
}
