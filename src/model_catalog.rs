use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use tokio::sync::watch;

use crate::{
    protocol::{
        AgentSelection, ModelCatalog, ModelDescriptor, ProviderCatalogStatus, ProviderId,
        ProviderModelCatalog, ProviderUnavailability, SettingsSnapshot,
    },
    provider::{ProviderError, ProviderRuntime, validate_models},
};

#[derive(Clone)]
pub(crate) struct ModelCatalogService {
    providers: Arc<Vec<ProviderCatalog>>,
}

#[derive(Clone)]
struct ProviderCatalog {
    runtime: Arc<dyn ProviderRuntime>,
    provider: ProviderId,
    display_name: String,
    state: Arc<Mutex<CatalogState>>,
    generation: watch::Sender<u64>,
    /// The effective Settings in force, read whenever this catalog is about to
    /// consult its Provider. A Provider the user turned off is never asked for
    /// its Models, so no process starts on its behalf.
    settings: watch::Receiver<SettingsSnapshot>,
}

#[derive(Default)]
struct CatalogState {
    models: Option<Vec<ModelDescriptor>>,
    failure: Option<CatalogFailure>,
    refreshing: bool,
}

/// How the last discovery failed: what it said, and — when it failed because
/// the Provider cannot be used at all rather than because the catalog call
/// itself went wrong — the typed condition the user fixes outside Suru.
struct CatalogFailure {
    message: String,
    unavailable: Option<ProviderUnavailability>,
}

impl ModelCatalogService {
    /// Builds the catalog over `runtimes` and arms the watch that re-discovers
    /// a Provider the moment the user turns it back on. Arming is part of
    /// construction rather than a step a caller takes afterwards, because a
    /// caller that forgot it would get a service that quietly never re-consults
    /// an enabled Provider; the cost is that this needs a reactor to spawn on.
    pub(crate) fn new(
        runtimes: impl IntoIterator<Item = Arc<dyn ProviderRuntime>>,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        let service = Self {
            providers: Arc::new(
                runtimes
                    .into_iter()
                    .map(|runtime| ProviderCatalog {
                        provider: runtime.provider_id(),
                        display_name: runtime.display_name().to_owned(),
                        runtime,
                        state: Arc::new(Mutex::new(CatalogState::default())),
                        generation: watch::channel(0).0,
                        settings: settings.clone(),
                    })
                    .collect(),
            ),
        };
        service.refresh_providers_as_they_are_enabled(settings);
        service
    }

    /// Discovers a Provider's Models the moment the user turns it back on, so
    /// enabling it and using it are one step. A Provider disabled at startup
    /// was never consulted and has no cache to fall back on, so without this it
    /// would come back as a Provider with no Models until something asked
    /// again.
    fn refresh_providers_as_they_are_enabled(
        &self,
        mut settings: watch::Receiver<SettingsSnapshot>,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut was_enabled = service
                .providers
                .iter()
                .map(ProviderCatalog::is_enabled)
                .collect::<Vec<_>>();
            while settings.changed().await.is_ok() {
                for (catalog, was_enabled) in service.providers.iter().zip(was_enabled.iter_mut()) {
                    let is_enabled = catalog.is_enabled();
                    if is_enabled && !*was_enabled {
                        catalog.begin_refresh();
                    }
                    *was_enabled = is_enabled;
                }
            }
        });
    }

    pub(crate) async fn list(&self) -> ModelCatalog {
        ModelCatalog {
            providers: join_all(self.providers.iter().map(ProviderCatalog::list)).await,
        }
    }

    pub(crate) async fn refresh(&self) -> ModelCatalog {
        ModelCatalog {
            providers: join_all(self.providers.iter().map(ProviderCatalog::refresh)).await,
        }
    }

    /// The Agent Selection a fresh Landing starts from: hosted Providers are
    /// consulted in their fixed built-in order, and the first selectable one
    /// whose cached catalog carries a default Model supplies it. A Provider the
    /// user cannot use yet — or has turned off — is passed over, so a first
    /// Prompt never lands on a Provider that will not work.
    pub(crate) fn default_selection(&self) -> Option<AgentSelection> {
        self.providers
            .iter()
            .find_map(ProviderCatalog::default_selection)
    }

    pub(crate) fn normalize_selection(
        &self,
        selection: &AgentSelection,
    ) -> Result<AgentSelection, String> {
        let Some(model) = self.cached_models(&selection.provider).and_then(|models| {
            models
                .iter()
                .find(|model| model.id == selection.model)
                .cloned()
        }) else {
            return Ok(selection.clone());
        };
        model
            .materialize_agent_selection(Some(selection))
            .map_err(|error| error.to_string())
    }

    fn cached_models(&self, provider: &ProviderId) -> Option<Vec<ModelDescriptor>> {
        self.providers
            .iter()
            .find(|catalog| &catalog.provider == provider)?
            .state
            .lock()
            .expect("Model catalog lock is not poisoned")
            .models
            .clone()
    }
}

impl ProviderCatalog {
    /// Whether the user has left this Provider on. Everything that would
    /// consult the runtime asks here first.
    fn is_enabled(&self) -> bool {
        self.settings
            .borrow()
            .settings
            .provider_enabled(&self.provider)
    }

    /// What a Provider the user turned off reports: no Models, because none
    /// were asked for. Whatever it discovered before a disable stays cached, so
    /// a disable/enable round-trip costs nothing — it is simply not on offer
    /// while the Provider is off.
    fn disabled_catalog(&self) -> ProviderModelCatalog {
        ProviderModelCatalog {
            provider: self.provider.clone(),
            display_name: self.display_name.clone(),
            models: Vec::new(),
            status: ProviderCatalogStatus::Disabled,
        }
    }

    /// The selection this Provider's cached default Model makes, or nothing
    /// while the Provider cannot be used or the user has turned it off.
    fn default_selection(&self) -> Option<AgentSelection> {
        if !self.is_enabled() {
            return None;
        }
        let state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        if state.is_unavailable() {
            return None;
        }
        state
            .models
            .as_ref()?
            .iter()
            .find(|model| model.is_default)
            .map(ModelDescriptor::default_agent_selection)
    }

    async fn list(&self) -> ProviderModelCatalog {
        if !self.is_enabled() {
            return self.disabled_catalog();
        }
        {
            let mut state = self
                .state
                .lock()
                .expect("Model catalog lock is not poisoned");
            if state.models.is_some() {
                self.begin_refresh_locked(&mut state);
                let status = catalog_status(&state);
                return self.snapshot(&state, status);
            }
        }
        self.refresh().await
    }

    async fn refresh(&self) -> ProviderModelCatalog {
        if !self.is_enabled() {
            return self.disabled_catalog();
        }
        let mut generation = self.generation.subscribe();
        self.begin_refresh();
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .expect("Model catalog lock is not poisoned");
                if !state.refreshing {
                    // Snapshot under the same lock acquisition that observed the
                    // settled refresh, so a concurrent `begin_refresh` cannot turn
                    // the status we waited for back into `Refreshing`.
                    let status = catalog_status(&state);
                    return self.snapshot(&state, status);
                }
            }
            if generation.changed().await.is_err() {
                return self.current();
            }
        }
    }

    fn begin_refresh(&self) {
        let mut state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        self.begin_refresh_locked(&mut state);
    }

    fn begin_refresh_locked(&self, state: &mut CatalogState) {
        if state.refreshing {
            return;
        }
        state.refreshing = true;
        let catalog = self.clone();
        tokio::spawn(async move { catalog.discover().await });
    }

    async fn discover(&self) {
        let result = self.runtime.list_models().await.and_then(|models| {
            validate_models(&models)?;
            if models.iter().any(|model| model.provider != self.provider) {
                return Err(ProviderError::new(format!(
                    "Provider `{}` returned a Model owned by another Provider",
                    self.provider
                )));
            }
            Ok(models)
        });
        let mut state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        match result {
            Ok(models) => {
                state.models = Some(models);
                state.failure = None;
            }
            Err(error) => {
                state.failure = Some(CatalogFailure {
                    message: error.to_string(),
                    unavailable: error.unavailability(),
                });
            }
        }
        state.refreshing = false;
        drop(state);
        self.generation.send_modify(|generation| *generation += 1);
    }

    fn current(&self) -> ProviderModelCatalog {
        let state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        let status = catalog_status(&state);
        self.snapshot(&state, status)
    }

    fn snapshot(
        &self,
        state: &CatalogState,
        status: ProviderCatalogStatus,
    ) -> ProviderModelCatalog {
        ProviderModelCatalog {
            provider: self.provider.clone(),
            display_name: self.display_name.clone(),
            models: state.models.clone().unwrap_or_default(),
            status,
        }
    }
}

impl CatalogState {
    fn is_unavailable(&self) -> bool {
        self.failure
            .as_ref()
            .is_some_and(|failure| failure.unavailable.is_some())
    }
}

fn catalog_status(state: &CatalogState) -> ProviderCatalogStatus {
    match (&state.models, &state.failure, state.refreshing) {
        // Unavailability outranks every other status, a refresh in flight
        // included: whatever Models the Provider served before, none of them
        // can be used until the user fixes the condition, and a client that
        // heard `Refreshing` in the meantime would offer them for the length of
        // the re-check.
        (
            _,
            Some(CatalogFailure {
                message,
                unavailable: Some(reason),
            }),
            _,
        ) => ProviderCatalogStatus::Unavailable {
            reason: *reason,
            message: message.clone(),
        },
        (_, _, true) => ProviderCatalogStatus::Refreshing,
        (Some(_), Some(failure), false) => ProviderCatalogStatus::Stale {
            message: failure.message.clone(),
        },
        (Some(_), None, false) => ProviderCatalogStatus::Fresh,
        (None, Some(failure), false) => ProviderCatalogStatus::Failed {
            message: failure.message.clone(),
        },
        (None, None, false) => ProviderCatalogStatus::Failed {
            message: "Model catalog has not been loaded".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::{
        protocol::{ModelAvailability, ModelId},
        provider::{ProviderFuture, ProviderSessionConnection, ProviderSessionRequest},
    };

    /// Serves one good catalog, then fails every later refresh.
    struct FailingAfterFirstRuntime {
        calls: AtomicUsize,
    }

    impl FailingAfterFirstRuntime {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ProviderRuntime for FailingAfterFirstRuntime {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new("stub")
        }

        fn display_name(&self) -> &str {
            "Stub"
        }

        fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    return Ok(vec![ModelDescriptor {
                        provider: ProviderId::new("stub"),
                        id: ModelId::new("stub-model"),
                        display_name: "Stub".to_owned(),
                        description: String::new(),
                        is_default: true,
                        availability: ModelAvailability::Available,
                        options: Vec::new(),
                    }]);
                }
                Err(ProviderError::new("temporary catalog outage"))
            })
        }

        fn start_session(
            &self,
            _request: ProviderSessionRequest,
        ) -> ProviderFuture<'_, ProviderSessionConnection> {
            unimplemented!("catalog tests never start Sessions")
        }

        fn run_errand(
            &self,
            _errand: crate::provider::ProviderErrand,
        ) -> ProviderFuture<'_, serde_json::Value> {
            unimplemented!("catalog tests never run Errands")
        }

        fn shutdown(&self) -> ProviderFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Issue #83: `refresh` must report the settled status of the refresh it waited
    /// for, even when concurrent `list` calls immediately re-arm a new background
    /// refresh between the moment the awaited refresh settles and the moment the
    /// caller reads the status.
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_reports_the_settled_status_despite_concurrent_list_rearming() {
        let (settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let service = ModelCatalogService::new(
            [Arc::new(FailingAfterFirstRuntime::new()) as Arc<dyn ProviderRuntime>],
            settings_rx,
        );
        assert_eq!(
            service.refresh().await.providers[0].status,
            ProviderCatalogStatus::Fresh
        );

        let stop = Arc::new(AtomicBool::new(false));
        let hammers: Vec<_> = (0..4)
            .map(|_| {
                let service = service.clone();
                let stop = Arc::clone(&stop);
                tokio::spawn(async move {
                    while !stop.load(Ordering::SeqCst) {
                        service.list().await;
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();

        for _ in 0..500 {
            let status = &service.refresh().await.providers[0].status;
            assert!(
                matches!(
                    status,
                    ProviderCatalogStatus::Stale { message } if message.contains("temporary catalog outage")
                ),
                "refresh must return the settled status it waited for, got {status:?}"
            );
        }

        stop.store(true, Ordering::SeqCst);
        for hammer in hammers {
            hammer.await.expect("hammer task completes");
        }
        drop(settings);
    }
}
