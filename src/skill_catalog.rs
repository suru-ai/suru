//! Server-owned live Skill Catalog authority and Prompt binding admission.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::{broadcast, watch};

use crate::{
    protocol::{
        InitialPrompt, ProviderId, SettingsSnapshot, SkillCatalog, SkillCatalogCapabilities,
        SkillCatalogRequest, SkillCatalogStatus, SkillId, SkillPromptDelivery, Workspace,
        skill_marker_matches,
    },
    provider::ProviderRuntime,
};

const UPDATE_BUFFER: usize = 64;

#[derive(Clone)]
pub(crate) struct SkillCatalogService {
    inner: Arc<SkillCatalogInner>,
}

struct SkillCatalogInner {
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    settings: watch::Receiver<SettingsSnapshot>,
    cache: Mutex<HashMap<CatalogKey, CacheEntry>>,
    dirty_providers: Mutex<HashSet<ProviderId>>,
    updates: broadcast::Sender<SkillCatalog>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CatalogKey {
    provider: ProviderId,
    workspace: PathBuf,
}

struct CacheEntry {
    catalog: SkillCatalog,
    generation: u64,
    in_flight: bool,
    refresh_pending: bool,
}

#[derive(Clone, Copy)]
enum DiscoveryMode {
    Cached,
    ForceRefresh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SkillCatalogError {
    InvalidWorkspace,
    ProviderNotHosted(ProviderId),
    InvalidCatalog(String),
    InvalidInvocation(String),
}

impl SkillCatalogService {
    pub(crate) fn new(
        runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let service = Self {
            inner: Arc::new(SkillCatalogInner {
                runtimes: runtimes.clone(),
                settings,
                cache: Mutex::new(HashMap::new()),
                dirty_providers: Mutex::new(HashSet::new()),
                updates,
            }),
        };
        for runtime in runtimes.iter() {
            let Some(mut invalidations) = runtime.subscribe_skill_catalog_invalidations() else {
                continue;
            };
            let service = service.clone();
            let provider = runtime.provider_id();
            tokio::spawn(async move {
                while invalidations.changed().await.is_ok() {
                    service.invalidate_provider(&provider);
                }
            });
        }
        service
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SkillCatalog> {
        self.inner.updates.subscribe()
    }

    /// Reads server authority. A cold context immediately returns Loading and
    /// starts one background discovery; concurrent readers share that work.
    pub(crate) async fn list(
        &self,
        request: SkillCatalogRequest,
    ) -> Result<SkillCatalog, SkillCatalogError> {
        let key = self.resolve_key(request)?;
        if let Some(catalog) = self.disabled_catalog(&key) {
            return Ok(catalog);
        }
        let runtime = self.runtime(&key.provider)?;
        let mut cache = self
            .inner
            .cache
            .lock()
            .expect("Skill Catalog cache lock is not poisoned");
        if let Some(entry) = cache.get(&key) {
            return Ok(entry.catalog.clone());
        }
        let catalog = loading_catalog(&key);
        cache.insert(
            key.clone(),
            CacheEntry {
                catalog: catalog.clone(),
                generation: 1,
                in_flight: true,
                refresh_pending: false,
            },
        );
        drop(cache);
        let mode = if self
            .inner
            .dirty_providers
            .lock()
            .expect("dirty Provider lock is not poisoned")
            .contains(&key.provider)
        {
            DiscoveryMode::ForceRefresh
        } else {
            DiscoveryMode::Cached
        };
        self.spawn_discovery(key, runtime, 1, mode);
        Ok(catalog)
    }

    /// Invalidates exactly one context and returns its transitional state.
    /// Native force refresh and user retry both pass through this operation.
    pub(crate) async fn refresh(
        &self,
        request: SkillCatalogRequest,
    ) -> Result<SkillCatalog, SkillCatalogError> {
        let key = self.resolve_key(request)?;
        if let Some(catalog) = self.disabled_catalog(&key) {
            return Ok(catalog);
        }
        let runtime = self.runtime(&key.provider)?;
        Ok(self.begin_refresh(key, runtime, DiscoveryMode::ForceRefresh))
    }

    pub(crate) async fn validate_prompt(
        &self,
        provider: ProviderId,
        workspace: &Path,
        prompt: &InitialPrompt,
        delivery: SkillPromptDelivery,
    ) -> Result<(), SkillCatalogError> {
        if prompt.skill_invocations.is_empty() {
            return Ok(());
        }
        let key = self.resolve_key(SkillCatalogRequest {
            provider,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
        })?;
        let catalog = self.await_current(key).await?;
        if !matches!(catalog.status, SkillCatalogStatus::Fresh { .. }) {
            return Err(SkillCatalogError::InvalidInvocation(
                "the Provider's Skill Catalog is not current".to_owned(),
            ));
        }
        if !catalog
            .capabilities
            .supported_deliveries
            .contains(&delivery)
        {
            let guidance = self
                .runtime(&catalog.provider)?
                .skill_delivery_rejection_guidance(delivery)
                .map(|guidance| format!("; {guidance}"))
                .unwrap_or_default();
            return Err(SkillCatalogError::InvalidInvocation(format!(
                "the Provider does not support Skill Invocations for {delivery:?} Prompts{guidance}"
            )));
        }

        let distinct = prompt
            .skill_invocations
            .iter()
            .map(|invocation| &invocation.skill_id)
            .collect::<HashSet<_>>();
        if catalog
            .capabilities
            .max_distinct_invocations
            .is_some_and(|limit| distinct.len() > limit as usize)
        {
            return Err(SkillCatalogError::InvalidInvocation(
                "the Prompt invokes more distinct Skills than the Provider supports".to_owned(),
            ));
        }

        for invocation in &prompt.skill_invocations {
            let descriptor = catalog
                .skills
                .iter()
                .find(|skill| skill.id == invocation.skill_id)
                .ok_or_else(|| {
                    SkillCatalogError::InvalidInvocation(format!(
                        "Skill identity `{}` is not offered in this Provider and Workspace",
                        invocation.skill_id
                    ))
                })?;
            if descriptor.name != invocation.name || descriptor.scope != invocation.scope {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill identity `{}` does not match its safe metadata",
                    invocation.skill_id
                )));
            }
            let start = invocation.marker.start as usize;
            let end = invocation.marker.end as usize;
            let Some(marker) = prompt.text.get(start..end) else {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill `{}` has an invalid marker range",
                    invocation.name
                )));
            };
            if !skill_marker_matches(marker, &descriptor.name) {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill `{}` is not bound to its visible marker",
                    invocation.name
                )));
            }
        }
        Ok(())
    }

    /// Waits only when authority for this context is already being established
    /// or refreshed. This keeps manually constructed bindings and cold clients
    /// on the same authoritative path as prefetched composers without turning
    /// ordinary Prompts into Skill discovery work.
    async fn await_current(&self, key: CatalogKey) -> Result<SkillCatalog, SkillCatalogError> {
        let mut updates = self.subscribe();
        let mut catalog = self
            .list(SkillCatalogRequest {
                provider: key.provider.clone(),
                workspace: Workspace {
                    path: key.workspace.clone(),
                },
            })
            .await?;
        while matches!(
            catalog.status,
            SkillCatalogStatus::Loading | SkillCatalogStatus::Refreshing
        ) {
            match updates.recv().await {
                Ok(update)
                    if update.provider == key.provider
                        && update.workspace.path == key.workspace =>
                {
                    catalog = update;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(current) = self
                        .inner
                        .cache
                        .lock()
                        .expect("Skill Catalog cache lock is not poisoned")
                        .get(&key)
                        .map(|entry| entry.catalog.clone())
                    {
                        catalog = current;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(SkillCatalogError::InvalidInvocation(
                        "the Provider's Skill Catalog stopped updating".to_owned(),
                    ));
                }
            }
        }
        Ok(catalog)
    }

    fn resolve_key(&self, request: SkillCatalogRequest) -> Result<CatalogKey, SkillCatalogError> {
        self.runtime(&request.provider)?;
        Ok(CatalogKey {
            provider: request.provider,
            workspace: canonical_workspace(&request.workspace.path)?,
        })
    }

    fn runtime(
        &self,
        provider: &ProviderId,
    ) -> Result<Arc<dyn ProviderRuntime>, SkillCatalogError> {
        self.inner
            .runtimes
            .iter()
            .find(|runtime| runtime.provider_id() == *provider)
            .cloned()
            .ok_or_else(|| SkillCatalogError::ProviderNotHosted(provider.clone()))
    }

    fn disabled_catalog(&self, key: &CatalogKey) -> Option<SkillCatalog> {
        (!self
            .inner
            .settings
            .borrow()
            .settings
            .provider_enabled(&key.provider))
        .then(|| unavailable_catalog(key, "Provider is disabled"))
    }

    fn invalidate_provider(&self, provider: &ProviderId) {
        self.inner
            .dirty_providers
            .lock()
            .expect("dirty Provider lock is not poisoned")
            .insert(provider.clone());
        let keys = self
            .inner
            .cache
            .lock()
            .expect("Skill Catalog cache lock is not poisoned")
            .keys()
            .filter(|key| &key.provider == provider)
            .cloned()
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return;
        }
        let Ok(runtime) = self.runtime(provider) else {
            return;
        };
        for key in keys {
            self.begin_refresh(key, runtime.clone(), DiscoveryMode::ForceRefresh);
        }
    }

    fn begin_refresh(
        &self,
        key: CatalogKey,
        runtime: Arc<dyn ProviderRuntime>,
        mode: DiscoveryMode,
    ) -> SkillCatalog {
        let mut cache = self
            .inner
            .cache
            .lock()
            .expect("Skill Catalog cache lock is not poisoned");
        if let Some(entry) = cache.get_mut(&key)
            && entry.in_flight
        {
            if matches!(mode, DiscoveryMode::ForceRefresh) {
                entry.refresh_pending = true;
            }
            return entry.catalog.clone();
        }
        let (catalog, generation) = if let Some(entry) = cache.get_mut(&key) {
            entry.generation = entry.generation.saturating_add(1);
            entry.in_flight = true;
            entry.catalog.status = SkillCatalogStatus::Refreshing;
            (entry.catalog.clone(), entry.generation)
        } else {
            let catalog = loading_catalog(&key);
            cache.insert(
                key.clone(),
                CacheEntry {
                    catalog: catalog.clone(),
                    generation: 1,
                    in_flight: true,
                    refresh_pending: false,
                },
            );
            (catalog, 1)
        };
        drop(cache);
        let _ = self.inner.updates.send(catalog.clone());
        self.spawn_discovery(key, runtime, generation, mode);
        catalog
    }

    fn spawn_discovery(
        &self,
        key: CatalogKey,
        runtime: Arc<dyn ProviderRuntime>,
        generation: u64,
        mode: DiscoveryMode,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            let result = match mode {
                DiscoveryMode::Cached => runtime.skill_catalog(&key.workspace).await,
                DiscoveryMode::ForceRefresh => runtime.refresh_skill_catalog(&key.workspace).await,
            };
            service.finish_discovery(key, generation, mode, result);
        });
    }

    fn finish_discovery(
        &self,
        key: CatalogKey,
        generation: u64,
        mode: DiscoveryMode,
        result: Result<SkillCatalog, crate::provider::ProviderError>,
    ) {
        let result = result.and_then(|catalog| {
            validate_catalog(&catalog, &key.provider, &key.workspace)
                .map(|()| catalog)
                .map_err(|error| crate::provider::ProviderError::new(format!("{error:?}")))
        });
        let mut cache = self
            .inner
            .cache
            .lock()
            .expect("Skill Catalog cache lock is not poisoned");
        let Some(entry) = cache.get_mut(&key) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        if entry.refresh_pending {
            entry.refresh_pending = false;
            entry.generation = entry.generation.saturating_add(1);
            let next_generation = entry.generation;
            drop(cache);
            let runtime = self
                .runtime(&key.provider)
                .expect("a cached Skill Catalog's Provider remains hosted");
            self.spawn_discovery(key, runtime, next_generation, DiscoveryMode::ForceRefresh);
            return;
        }
        let clears_dirty_provider = matches!(mode, DiscoveryMode::ForceRefresh) && result.is_ok();
        entry.in_flight = false;
        match result {
            Ok(catalog) => entry.catalog = catalog,
            Err(error) => {
                tracing::warn!(provider = %key.provider, "Skill discovery failed: {error}");
                if entry.catalog.skills.is_empty() {
                    entry.catalog = unavailable_catalog(
                        &key,
                        "Skills are unavailable because discovery failed",
                    );
                } else {
                    entry.catalog.status = SkillCatalogStatus::Stale {
                        message: "Skills are unavailable because refresh failed".to_owned(),
                    };
                }
            }
        }
        let catalog = entry.catalog.clone();
        drop(cache);
        if clears_dirty_provider {
            self.inner
                .dirty_providers
                .lock()
                .expect("dirty Provider lock is not poisoned")
                .remove(&key.provider);
        }
        let _ = self.inner.updates.send(catalog);
    }
}

fn loading_catalog(key: &CatalogKey) -> SkillCatalog {
    SkillCatalog {
        provider: key.provider.clone(),
        workspace: Workspace {
            path: key.workspace.clone(),
        },
        skills: Vec::new(),
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: Vec::new(),
        },
        status: SkillCatalogStatus::Loading,
    }
}

fn unavailable_catalog(key: &CatalogKey, message: &str) -> SkillCatalog {
    SkillCatalog {
        provider: key.provider.clone(),
        workspace: Workspace {
            path: key.workspace.clone(),
        },
        skills: Vec::new(),
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: Some(0),
            supported_deliveries: Vec::new(),
        },
        status: SkillCatalogStatus::Unavailable {
            message: message.to_owned(),
        },
    }
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, SkillCatalogError> {
    let workspace = fs::canonicalize(workspace).map_err(|_| SkillCatalogError::InvalidWorkspace)?;
    workspace
        .is_dir()
        .then_some(workspace)
        .ok_or(SkillCatalogError::InvalidWorkspace)
}

fn validate_catalog(
    catalog: &SkillCatalog,
    provider: &ProviderId,
    workspace: &Path,
) -> Result<(), SkillCatalogError> {
    if &catalog.provider != provider {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned a Skill Catalog owned by another Provider".to_owned(),
        ));
    }
    if catalog.workspace.path != workspace {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned a Skill Catalog for another Workspace".to_owned(),
        ));
    }
    let mut identities = HashSet::<&SkillId>::new();
    if catalog.skills.iter().any(|skill| {
        skill.name.is_empty()
            || skill.name.chars().any(char::is_whitespace)
            || !identities.insert(&skill.id)
    }) {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned invalid or duplicate Skill metadata".to_owned(),
        ));
    }
    Ok(())
}
