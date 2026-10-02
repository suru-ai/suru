//! Changing a Setting: the one operation through which both the settings
//! panel's change and a Sidekick's `set_setting` edit the Config Document and
//! put what it then says in force, so a Setting a Sidekick changed takes
//! effect and reaches every Client exactly as one the user changed does.
//!
//! The operation changes whatever Setting it is given. What a Sidekick may
//! change is bounded where its Tool is served, over the schema (ADR 0043),
//! since the user's own panel may change every Setting there is.

use std::sync::Arc;

use tokio::sync::watch;

use super::SessionOperations;
use crate::{
    protocol::{SettingMutation, SettingsSnapshot},
    provider::ProviderRuntime,
    serving::ServingController,
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
        }
    }
}

/// Why a Setting was not changed, or was changed without all of Suru
/// following it.
#[derive(Debug)]
pub(crate) enum SettingRefusal {
    /// The Config Document could not take the edit, so nothing changed.
    Document(SettingsMutationError),
    /// The edit landed and is in force everywhere else, but the Serving
    /// listener could not be moved to what the Serving Settings now say.
    Serving(anyhow::Error),
}

impl std::fmt::Display for SettingRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(error) => write!(formatter, "Nothing was changed: {error}."),
            Self::Serving(error) => write!(
                formatter,
                "The Setting was changed, but the Serving listener could not follow it: {error}."
            ),
        }
    }
}

impl SessionOperations {
    /// Applies `mutation` to the winning Config Document and puts the
    /// settings the reloaded document yields in force: every hosted Provider
    /// runtime takes the Server Settings it now runs under, the Serving
    /// listener moves to what its Settings say, every attached Client is
    /// pushed the snapshot, and each live Session following a Provider's
    /// Approval Posture Settings takes the posture they now make. Answers with
    /// that snapshot.
    pub(crate) async fn change_setting(
        &self,
        mutation: SettingMutation,
    ) -> Result<SettingsSnapshot, SettingRefusal> {
        let adoption = &self.settings_adoption;
        // The edit is filesystem work, and the CST handles it parses the
        // document into are not `Send`; both stay on a blocking thread, where
        // the read, the edit, and the write are one scope.
        let documents = adoption.documents.clone();
        let snapshot = tokio::task::spawn_blocking(move || documents.mutate(&mutation))
            .await
            .expect("Config Document edit runs to completion")
            .map_err(|error| {
                if let SettingsMutationError::Io { .. } = error {
                    tracing::error!("Setting mutation failed: {error}");
                }
                SettingRefusal::Document(error)
            })?;
        if let Err(error) = crate::server::adopt_settings(
            &adoption.snapshot,
            &adoption.runtimes,
            &adoption.serving,
            &snapshot,
        )
        .await
        {
            tracing::error!("could not adopt Serving settings: {error:#}");
            return Err(SettingRefusal::Serving(error));
        }
        let changed = self
            .sessions
            .reconcile_approval_postures(&snapshot.settings);
        crate::server::apply_live_posture_updates(&self.providers, changed).await;
        Ok(snapshot)
    }
}
