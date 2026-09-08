use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use tokio::sync::watch;

use crate::{
    protocol::{
        AgentSelection, ModelAvailability, ModelCatalog, ModelDescriptor, ProviderCatalogStatus,
        ProviderId, ProviderModelCatalog, ProviderUnavailability, SessionTimestamp,
        SettingsSnapshot,
    },
    provider::{ProviderError, ProviderRuntime, validate_models},
};

/// The Model Catalog one Provider served the last time discovery succeeded,
/// as Suru remembers it across restarts. Only a successful discovery is
/// remembered: a failure or an unavailability is a fact about the environment
/// Suru re-reads rather than replays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RememberedProviderCatalog {
    pub(crate) provider: ProviderId,
    pub(crate) models: Vec<ModelDescriptor>,
    pub(crate) warning: Option<String>,
    pub(crate) discovered_at: SessionTimestamp,
}

/// Where a remembered catalog is kept: what was remembered before this
/// process started, and where each successful discovery is written so the
/// next process starts from it.
pub(crate) struct CatalogMemory {
    pub(crate) remembered: Vec<RememberedProviderCatalog>,
    pub(crate) remember: Option<RememberCatalog>,
}

pub(crate) type RememberCatalog = Arc<dyn Fn(RememberedProviderCatalog) + Send + Sync>;

impl CatalogMemory {
    /// A memory that remembers nothing and is written nowhere, for a service
    /// that lives only as long as its process.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            remembered: Vec::new(),
            remember: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ModelCatalogService {
    providers: Arc<Vec<ProviderCatalog>>,
    /// Bumped whenever what [`Self::current`] would answer changes: a
    /// discovery settling, or a Provider's Enablement turning. A stream
    /// pushing the catalog to clients waits on it.
    changes: watch::Sender<u64>,
}

#[derive(Clone)]
struct ProviderCatalog {
    runtime: Arc<dyn ProviderRuntime>,
    provider: ProviderId,
    display_name: String,
    state: Arc<Mutex<CatalogState>>,
    generation: watch::Sender<u64>,
    changes: watch::Sender<u64>,
    remember: Option<RememberCatalog>,
    /// The effective Settings in force, read whenever this catalog is about to
    /// consult its Provider. A Provider the user turned off is never asked for
    /// its Models, so no process starts on its behalf.
    settings: watch::Receiver<SettingsSnapshot>,
}

#[derive(Default)]
struct CatalogState {
    models: Option<Vec<ModelDescriptor>>,
    warning: Option<String>,
    failure: Option<CatalogFailure>,
    refreshing: bool,
    /// Whether this process has heard the Provider's own answer yet, whatever
    /// it was: a failure is an answer too, and one to re-read when the user
    /// asks rather than on every connect. A remembered catalog serves Models
    /// without setting this, which is what lets a connecting client ask for
    /// the live answer exactly once.
    discovered_live: bool,
}

impl CatalogState {
    fn remembered(remembered: RememberedProviderCatalog) -> Self {
        Self {
            models: Some(remembered.models),
            warning: remembered.warning,
            ..Self::default()
        }
    }
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
    ///
    /// `memory` seeds each Provider with the catalog remembered from the last
    /// process, so Models are served by name before any Provider is asked. A
    /// remembered catalog that fails the same checks a live one must pass, or
    /// names a Provider this server does not host, is dropped rather than
    /// served.
    pub(crate) fn new(
        runtimes: impl IntoIterator<Item = Arc<dyn ProviderRuntime>>,
        settings: watch::Receiver<SettingsSnapshot>,
        memory: CatalogMemory,
    ) -> Self {
        let CatalogMemory {
            mut remembered,
            remember,
        } = memory;
        let (changes, _) = watch::channel(0);
        let service = Self {
            providers: Arc::new(
                runtimes
                    .into_iter()
                    .map(|runtime| {
                        let provider = runtime.provider_id();
                        let state = remembered
                            .iter()
                            .position(|candidate| candidate.provider == provider)
                            .map(|index| remembered.swap_remove(index))
                            .filter(|candidate| {
                                match admissible_discovery(&provider, &candidate.models) {
                                    Ok(()) => true,
                                    Err(error) => {
                                        tracing::warn!(
                                            %provider,
                                            "dropping the remembered Model Catalog: {error}"
                                        );
                                        false
                                    }
                                }
                            })
                            .map_or_else(CatalogState::default, CatalogState::remembered);
                        ProviderCatalog {
                            provider,
                            display_name: runtime.display_name().to_owned(),
                            runtime,
                            state: Arc::new(Mutex::new(state)),
                            generation: watch::channel(0).0,
                            changes: changes.clone(),
                            remember: remember.clone(),
                            settings: settings.clone(),
                        }
                    })
                    .collect(),
            ),
            changes,
        };
        service.refresh_providers_as_they_are_enabled(settings);
        service
    }

    /// The catalog as it stands, without asking any Provider. What a
    /// connecting client is shown first, and what it is shown again each time
    /// [`Self::subscribe`] reports a change.
    pub(crate) fn current(&self) -> ModelCatalog {
        ModelCatalog {
            providers: self
                .providers
                .iter()
                .map(ProviderCatalog::current_or_disabled)
                .collect(),
        }
    }

    /// Announces every change to what [`Self::current`] answers.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Asks each enabled Provider for its Models if this process has not yet
    /// heard its answer. A client connecting to a server that so far serves
    /// only what it remembered is what calls this; a Provider already
    /// discovered live is left alone, so reconnecting clients cost nothing.
    pub(crate) fn warm(&self) {
        for catalog in self.providers.iter() {
            if !catalog.is_enabled() {
                continue;
            }
            let mut state = catalog
                .state
                .lock()
                .expect("Model catalog lock is not poisoned");
            if !state.discovered_live {
                catalog.begin_refresh_locked(&mut state);
            }
        }
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
                let mut turned = false;
                for (catalog, was_enabled) in service.providers.iter().zip(was_enabled.iter_mut()) {
                    let is_enabled = catalog.is_enabled();
                    if is_enabled && !*was_enabled {
                        catalog.begin_refresh();
                    }
                    turned |= is_enabled != *was_enabled;
                    *was_enabled = is_enabled;
                }
                if turned {
                    service.changes.send_modify(|changes| *changes += 1);
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

    /// The Agent Selection an Errand at `provider` runs under: the Errand
    /// Selection that Provider declares, and its default Model when the
    /// declaration names something its catalog no longer carries. Nothing while
    /// it has neither, which is the point at which whoever asked for the Errand
    /// gives up on it rather than guessing at a Model.
    ///
    /// `pinned` is a Selection the caller insists on — a Setting pinning one
    /// for every Errand of its purpose — which stands in for the Provider's own
    /// declaration and is then resolved by exactly the same rules. A user who
    /// pins a Model has said which Model to prefer, not that the Errand should
    /// fail once that Model goes.
    ///
    /// This resolves afresh on every Errand rather than once at startup,
    /// because a Provider's catalog changes underneath a running server and a
    /// Model that has gone should cost one Errand its cheapness rather than
    /// cost every Errand its answer.
    pub(crate) async fn resolved_errand_selection(
        &self,
        provider: &ProviderId,
        pinned: Option<&AgentSelection>,
    ) -> Option<AgentSelection> {
        let catalog = self
            .providers
            .iter()
            .find(|catalog| &catalog.provider == provider)?;
        catalog.resolved_errand_selection(pinned).await
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
        default_model(state.models.as_ref()?).map(ModelDescriptor::default_agent_selection)
    }

    /// The Agent Selection an Errand at this Provider runs under, resolved
    /// against the Models it is serving right now. It is named for the
    /// resolution rather than the declaration because the two differ: what the
    /// runtime declares is one input, and a Provider that has withdrawn it
    /// answers here with its default Model instead.
    ///
    /// A catalog already discovered is never refreshed for an Errand's sake.
    /// One never discovered is loaded, because the alternative is that every
    /// Errand asked before a client first listed Models resolves against an
    /// empty catalog and is skipped.
    ///
    /// Enablement is deliberately not read here, unlike in
    /// [`Self::default_selection`]: a Provider the user turned off is refused
    /// where the Errand is run, which is the one place that can say so, and
    /// discovery declines to consult it either way. A Provider that is off
    /// therefore either resolves a Selection and is refused by name, or — never
    /// having been discovered — offers no Model, which is what a Provider that
    /// is off does.
    async fn resolved_errand_selection(
        &self,
        pinned: Option<&AgentSelection>,
    ) -> Option<AgentSelection> {
        self.discover_once().await;
        // A Selection the user pinned stands in front of the Provider's own
        // declaration outright rather than beside it: the Provider declares
        // what it would choose, and the user has said otherwise.
        let declared = pinned.cloned().or_else(|| self.runtime.errand_selection());
        let state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        if state.is_unavailable() {
            return None;
        }
        errand_selection_in(state.models.as_ref()?, declared)
    }

    /// Asks this Provider for its Models if it has never been asked. A Provider
    /// the user has turned off is not asked at all, here as anywhere.
    async fn discover_once(&self) {
        if self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned")
            .models
            .is_some()
        {
            return;
        }
        self.refresh().await;
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
        let result = self.runtime.list_models().await.and_then(|discovery| {
            admissible_discovery(&self.provider, &discovery.models)?;
            Ok(discovery)
        });
        // What to remember is decided before the lock and written after it:
        // a memory is whatever the caller made it, and it must never run under
        // the catalog's own lock.
        let remembered = match (&self.remember, &result) {
            (Some(_), Ok(discovery)) => Some(RememberedProviderCatalog {
                provider: self.provider.clone(),
                models: discovery.models.clone(),
                warning: discovery.warning.clone(),
                discovered_at: SessionTimestamp::now(),
            }),
            _ => None,
        };
        let mut state = self
            .state
            .lock()
            .expect("Model catalog lock is not poisoned");
        match result {
            Ok(discovery) => {
                state.models = Some(discovery.models);
                state.warning = discovery.warning;
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
        state.discovered_live = true;
        drop(state);
        if let (Some(remember), Some(remembered)) = (&self.remember, remembered) {
            remember(remembered);
        }
        self.generation.send_modify(|generation| *generation += 1);
        self.changes.send_modify(|changes| *changes += 1);
    }

    /// What this Provider serves right now, or the disabled catalog while the
    /// user has it off. Asks nothing of the Provider either way.
    fn current_or_disabled(&self) -> ProviderModelCatalog {
        if !self.is_enabled() {
            return self.disabled_catalog();
        }
        self.current()
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

/// The checks every catalog must pass before it is served, whether a Provider
/// just answered with it or it was remembered from an earlier process.
fn admissible_discovery(
    provider: &ProviderId,
    models: &[ModelDescriptor],
) -> Result<(), ProviderError> {
    validate_models(models)?;
    if models.iter().any(|model| &model.provider != provider) {
        return Err(ProviderError::new(format!(
            "Provider `{provider}` returned a Model owned by another Provider"
        )));
    }
    Ok(())
}

/// The Model a Provider serves as its default, and nothing when it serves no
/// default.
fn default_model(models: &[ModelDescriptor]) -> Option<&ModelDescriptor> {
    models.iter().find(|model| model.is_default)
}

/// The Agent Selection an Errand runs under, given what a Provider is serving
/// and what — if anything — it declares its Errands run at.
///
/// The declaration is tried first and the Provider's default Model stands in
/// for one the catalog will not honor. The default has to be a Model the
/// Provider can actually run: falling back exists so an Errand still gets an
/// answer, and a Model the Provider is serving but cannot run is no better a
/// fallback than one that has gone. With neither, there is nothing to run an
/// Errand at and whoever asked for it gives up rather than guessing.
fn errand_selection_in(
    models: &[ModelDescriptor],
    declared: Option<AgentSelection>,
) -> Option<AgentSelection> {
    declared
        .and_then(|declared| admitted_selection(models, &declared))
        .or_else(|| {
            default_model(models)
                .filter(|model| model.availability == ModelAvailability::Available)
                .map(ModelDescriptor::default_agent_selection)
        })
}

/// A declared Errand Selection as the live catalog admits it: the declared
/// Model's own Option defaults with the declaration laid over the top, so a
/// runtime declares the one Option it has something to say about — the least
/// effort it publishes — and takes the Model's defaults for the rest.
///
/// A declaration the catalog cannot honor in full is not honored at all, and
/// gives way to the Provider's default Model. An Errand Selection is declared
/// as a whole and for cheapness, so a Model that has been withdrawn, an Option
/// it no longer carries, and a choice it no longer offers all mean the same
/// thing: this is no longer the Selection the Provider vouched for.
fn admitted_selection(
    models: &[ModelDescriptor],
    declared: &AgentSelection,
) -> Option<AgentSelection> {
    let model = models
        .iter()
        .find(|model| model.provider == declared.provider && model.id == declared.model)?;
    if model.availability != ModelAvailability::Available {
        return None;
    }
    let mut options = declared.options.clone();
    for default in model.default_agent_selection().options {
        if !options.iter().any(|option| option.id == default.id) {
            options.push(default);
        }
    }
    model
        .materialize_agent_selection(Some(&AgentSelection {
            provider: model.provider.clone(),
            model: model.id.clone(),
            options,
        }))
        .ok()
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
        (Some(_), None, false) => match &state.warning {
            Some(message) => ProviderCatalogStatus::Warning {
                message: message.clone(),
            },
            None => ProviderCatalogStatus::Fresh,
        },
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
        protocol::{
            ModelAvailability, ModelId, ModelOptionChoice, ModelOptionChoiceId,
            ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole,
            ModelOptionSelection, ModelOptionValue,
        },
        provider::{
            ProviderFuture, ProviderModelDiscovery, ProviderSessionConnection,
            ProviderSessionRequest,
        },
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

        fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    return Ok(ProviderModelDiscovery::new(vec![ModelDescriptor {
                        provider: ProviderId::new("stub"),
                        id: ModelId::new("stub-model"),
                        display_name: "Stub".to_owned(),
                        description: String::new(),
                        is_default: true,
                        availability: ModelAvailability::Available,
                        options: Vec::new(),
                    }]));
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

        fn errand_selection(&self) -> Option<AgentSelection> {
            None
        }

        fn shutdown(&self) -> ProviderFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    fn remembered(display_name: &str) -> RememberedProviderCatalog {
        RememberedProviderCatalog {
            provider: ProviderId::new("stub"),
            models: vec![ModelDescriptor {
                provider: ProviderId::new("stub"),
                id: ModelId::new("stub-model"),
                display_name: display_name.to_owned(),
                description: String::new(),
                is_default: true,
                availability: ModelAvailability::Available,
                options: Vec::new(),
            }],
            warning: None,
            discovered_at: SessionTimestamp(1),
        }
    }

    #[tokio::test]
    async fn a_remembered_catalog_serves_models_before_any_discovery() {
        let (_settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let runtime = Arc::new(FailingAfterFirstRuntime::new());
        let service = ModelCatalogService::new(
            [runtime.clone() as Arc<dyn ProviderRuntime>],
            settings_rx,
            CatalogMemory {
                remembered: vec![remembered("Remembered Stub")],
                remember: None,
            },
        );

        let catalog = service.current();
        assert_eq!(
            catalog.providers[0].models[0].display_name,
            "Remembered Stub"
        );
        assert_eq!(catalog.providers[0].status, ProviderCatalogStatus::Fresh);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            service.default_selection().map(|selection| selection.model),
            Some(ModelId::new("stub-model"))
        );
    }

    #[tokio::test]
    async fn warming_asks_each_provider_once_per_process() {
        let (_settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let runtime = Arc::new(FailingAfterFirstRuntime::new());
        let service = ModelCatalogService::new(
            [runtime.clone() as Arc<dyn ProviderRuntime>],
            settings_rx,
            CatalogMemory {
                remembered: vec![remembered("Remembered Stub")],
                remember: None,
            },
        );
        let mut changes = service.subscribe();

        service.warm();
        assert_eq!(
            service.current().providers[0].status,
            ProviderCatalogStatus::Refreshing
        );
        changes
            .changed()
            .await
            .expect("the catalog announces the settled discovery");
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        let catalog = service.current();
        assert_eq!(catalog.providers[0].status, ProviderCatalogStatus::Fresh);
        assert_eq!(catalog.providers[0].models[0].display_name, "Stub");

        service.warm();
        tokio::task::yield_now().await;
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            1,
            "a Provider discovered live this process is not asked again on connect"
        );
    }

    #[tokio::test]
    async fn a_provider_whose_discovery_failed_is_not_asked_again_on_connect() {
        let (_settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let runtime = Arc::new(FailingAfterFirstRuntime::new());
        let service = ModelCatalogService::new(
            [runtime.clone() as Arc<dyn ProviderRuntime>],
            settings_rx,
            CatalogMemory::none(),
        );
        service.refresh().await;
        assert_eq!(
            service.refresh().await.providers[0].status,
            ProviderCatalogStatus::Stale {
                message: "temporary catalog outage".to_owned()
            }
        );
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);

        service.warm();
        tokio::task::yield_now().await;
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            2,
            "a failure is an answer this process has heard; the user asks again, not every connect"
        );
    }

    #[tokio::test]
    async fn only_a_successful_discovery_is_remembered() {
        let (_settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let (remembered_tx, mut remembered_rx) = tokio::sync::mpsc::unbounded_channel();
        let service = ModelCatalogService::new(
            [Arc::new(FailingAfterFirstRuntime::new()) as Arc<dyn ProviderRuntime>],
            settings_rx,
            CatalogMemory {
                remembered: Vec::new(),
                remember: Some(Arc::new(move |catalog| {
                    let _ = remembered_tx.send(catalog);
                })),
            },
        );

        service.refresh().await;
        let remembered = remembered_rx
            .recv()
            .await
            .expect("the first discovery succeeds and is remembered");
        assert_eq!(remembered.provider, ProviderId::new("stub"));
        assert_eq!(remembered.models[0].display_name, "Stub");
        assert!(remembered.discovered_at.0 > 0);

        service.refresh().await;
        assert!(
            remembered_rx.try_recv().is_err(),
            "a failed discovery leaves what was remembered alone"
        );
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
            CatalogMemory::none(),
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

    const ERRAND_MODEL: &str = "cheap";

    fn choice(id: &str, availability: ModelAvailability) -> ModelOptionChoice {
        ModelOptionChoice {
            id: ModelOptionChoiceId::new(id),
            label: id.to_owned(),
            description: None,
            availability,
        }
    }

    fn select_option(
        id: &str,
        choices: Vec<ModelOptionChoice>,
        default: &str,
    ) -> ModelOptionDescriptor {
        ModelOptionDescriptor {
            id: ModelOptionId::new(id),
            label: id.to_owned(),
            description: None,
            role: ModelOptionRole::Other,
            kind: ModelOptionKind::Select {
                choices,
                default: ModelOptionChoiceId::new(default),
            },
        }
    }

    fn chosen(option: &str, choice: &str) -> ModelOptionSelection {
        ModelOptionSelection {
            id: ModelOptionId::new(option),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(choice),
            },
        }
    }

    /// The Model a Provider might declare its Errands run at: two Options, so a
    /// declaration can name one and leave the other alone.
    fn errand_model(availability: ModelAvailability) -> ModelDescriptor {
        ModelDescriptor {
            provider: ProviderId::new("stub"),
            id: ModelId::new(ERRAND_MODEL),
            display_name: "Errand".to_owned(),
            description: String::new(),
            is_default: false,
            availability,
            options: vec![
                select_option(
                    "effort",
                    vec![
                        choice("thorough", ModelAvailability::Available),
                        choice("brisk", ModelAvailability::Available),
                    ],
                    "thorough",
                ),
                select_option(
                    "verbosity",
                    vec![choice("plain", ModelAvailability::Available)],
                    "plain",
                ),
            ],
        }
    }

    /// The Model a Provider serves as its default, which is what an Errand runs
    /// at when the declaration cannot be honored.
    fn conversing_model(availability: ModelAvailability) -> ModelDescriptor {
        ModelDescriptor {
            provider: ProviderId::new("stub"),
            id: ModelId::new("conversing"),
            display_name: "Conversing".to_owned(),
            description: String::new(),
            is_default: true,
            availability,
            options: Vec::new(),
        }
    }

    fn declaration(options: Vec<ModelOptionSelection>) -> AgentSelection {
        AgentSelection {
            provider: ProviderId::new("stub"),
            model: ModelId::new(ERRAND_MODEL),
            options,
        }
    }

    /// A runtime declares the one Option it has something to say about — the
    /// least effort it publishes — and everything else it left unsaid comes
    /// from the Model, so a Provider adding an Option never invalidates a
    /// declaration written before it existed.
    #[test]
    fn a_declaration_is_laid_over_the_declared_models_own_defaults() {
        assert_eq!(
            errand_selection_in(
                &[
                    conversing_model(ModelAvailability::Available),
                    errand_model(ModelAvailability::Available),
                ],
                Some(declaration(vec![chosen("effort", "brisk")])),
            ),
            Some(declaration(vec![
                chosen("effort", "brisk"),
                chosen("verbosity", "plain"),
            ]))
        );
    }

    /// The Selection was declared as a whole and for cheapness, so a Provider
    /// that has withdrawn any part of it has withdrawn the vouching with it,
    /// and the Errand runs at that Provider's default Model instead.
    #[test]
    fn a_declaration_the_catalog_no_longer_carries_gives_way_to_the_default_model() {
        let served = |declared| {
            errand_selection_in(
                &[
                    conversing_model(ModelAvailability::Available),
                    errand_model(ModelAvailability::Available),
                ],
                Some(declared),
            )
        };
        let default =
            Some(conversing_model(ModelAvailability::Available).default_agent_selection());
        assert_eq!(
            served(declaration(vec![chosen("effort", "frugal")])),
            default,
            "an effort the Model no longer offers"
        );
        assert_eq!(
            served(declaration(vec![chosen("cadence", "brisk")])),
            default,
            "an Option the Model does not carry at all"
        );
        assert_eq!(
            errand_selection_in(
                &[conversing_model(ModelAvailability::Available)],
                Some(declaration(Vec::new())),
            ),
            default,
            "a Model that has gone from the catalog"
        );
        assert_eq!(
            errand_selection_in(
                &[
                    conversing_model(ModelAvailability::Available),
                    errand_model(ModelAvailability::Unavailable),
                ],
                Some(declaration(Vec::new())),
            ),
            default,
            "a Model the Provider is serving but cannot run"
        );
    }

    /// Falling back exists so an Errand still gets an answer, so a fallback that
    /// would not answer is no fallback: an Errand is skipped outright rather
    /// than sent to a Model the Provider cannot run or to no Model at all.
    #[test]
    fn a_provider_with_no_default_model_to_fall_back_to_runs_no_errand() {
        assert_eq!(
            errand_selection_in(
                &[conversing_model(ModelAvailability::Unavailable)],
                Some(declaration(Vec::new())),
            ),
            None,
            "a default Model the Provider is serving but cannot run"
        );
        assert_eq!(
            errand_selection_in(&[errand_model(ModelAvailability::Available)], None),
            None,
            "a catalog with no default Model in it at all"
        );
        assert_eq!(errand_selection_in(&[], None), None, "an empty catalog");
    }

    /// A Provider that declares nothing runs its Errands at the Model it
    /// already defaults to, which is what every built-in Provider does until it
    /// has something cheaper to name.
    #[test]
    fn a_provider_declaring_nothing_runs_errands_at_its_default_model() {
        assert_eq!(
            errand_selection_in(
                &[
                    conversing_model(ModelAvailability::Available),
                    errand_model(ModelAvailability::Available),
                ],
                None,
            ),
            Some(conversing_model(ModelAvailability::Available).default_agent_selection())
        );
    }
}
