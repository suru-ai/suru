use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use tokio::sync::watch;

use crate::{
    protocol::{
        AgentSelection, ModelCatalog, ModelDescriptor, ProviderCatalogStatus, ProviderId,
        ProviderModelCatalog,
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
    state: Arc<Mutex<CatalogState>>,
    generation: watch::Sender<u64>,
}

#[derive(Default)]
struct CatalogState {
    models: Option<Vec<ModelDescriptor>>,
    error: Option<String>,
    refreshing: bool,
}

impl ModelCatalogService {
    pub(crate) fn new(runtimes: impl IntoIterator<Item = Arc<dyn ProviderRuntime>>) -> Self {
        Self {
            providers: Arc::new(
                runtimes
                    .into_iter()
                    .map(|runtime| ProviderCatalog {
                        provider: runtime.provider_id(),
                        runtime,
                        state: Arc::new(Mutex::new(CatalogState::default())),
                        generation: watch::channel(0).0,
                    })
                    .collect(),
            ),
        }
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

    pub(crate) fn default_selection(&self, provider: &ProviderId) -> Option<AgentSelection> {
        self.cached_models(provider)?
            .iter()
            .find(|model| model.is_default)
            .map(ModelDescriptor::default_agent_selection)
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
    async fn list(&self) -> ProviderModelCatalog {
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
                state.error = None;
            }
            Err(error) => state.error = Some(error.to_string()),
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
            models: state.models.clone().unwrap_or_default(),
            status,
        }
    }
}

fn catalog_status(state: &CatalogState) -> ProviderCatalogStatus {
    match (&state.models, &state.error, state.refreshing) {
        (_, _, true) => ProviderCatalogStatus::Refreshing,
        (Some(_), Some(message), false) => ProviderCatalogStatus::Stale {
            message: message.clone(),
        },
        (Some(_), None, false) => ProviderCatalogStatus::Fresh,
        (None, Some(message), false) => ProviderCatalogStatus::Failed {
            message: message.clone(),
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
        let service = ModelCatalogService::new([
            Arc::new(FailingAfterFirstRuntime::new()) as Arc<dyn ProviderRuntime>
        ]);
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
    }
}
