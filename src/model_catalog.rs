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
        let cached = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned")
            .models
            .is_some();
        if cached {
            self.begin_refresh();
            let state = self
                .state
                .lock()
                .expect("Model catalog lock is not poisoned");
            let status = if state.refreshing {
                ProviderCatalogStatus::Refreshing
            } else {
                catalog_status(&state)
            };
            return self.snapshot(&state, status);
        }
        self.refresh().await
    }

    async fn refresh(&self) -> ProviderModelCatalog {
        let mut generation = self.generation.subscribe();
        self.begin_refresh();
        loop {
            if !self
                .state
                .lock()
                .expect("Model catalog lock is not poisoned")
                .refreshing
            {
                return self.current();
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
