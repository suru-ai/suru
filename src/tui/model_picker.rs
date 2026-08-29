//! Searchable Model picker state and stable catalog reconciliation.

use crate::protocol::{
    AgentSelection, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionKind,
    ModelOptionRole, ModelOptionValue, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
    ProviderUnavailability, SettingMutation,
};

use super::ModelListRequest;

/// What the Model a reader picks here is for. The picker is one list of Models
/// however it was opened; where its answer goes is the opener's question, asked
/// when the picker opens rather than guessed at when it closes.
#[derive(Clone, Copy, Debug, Default)]
pub(super) enum ModelPickerPurpose {
    /// The Agent this Session — or the Landing — will converse with.
    #[default]
    AgentSelection,
    /// One Setting holding an Agent Selection, pinned through the mutation the
    /// Setting itself supplied. The picker knows nothing about which Setting:
    /// it carries the pin and hands the choice to it.
    Setting(fn(AgentSelection) -> SettingMutation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelKey {
    provider: ProviderId,
    model: ModelId,
}

impl ModelKey {
    fn new(provider: ProviderId, model: ModelId) -> Self {
        Self { provider, model }
    }

    fn from_model(model: &ModelDescriptor) -> Self {
        Self::new(model.provider.clone(), model.id.clone())
    }

    fn from_selection(selection: &AgentSelection) -> Self {
        Self::new(selection.provider.clone(), selection.model.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerSelection {
    Model(ModelKey),
    Retry(ProviderId),
}

#[derive(Clone, Debug)]
struct ProviderModels {
    provider: ProviderId,
    /// The Provider's name as the user reads it. Catalog-served rows carry the
    /// runtime-declared name; rows the picker invents for a Provider no catalog
    /// answered for fall back to the wire identifier, the only name it has.
    display_name: String,
    models: Vec<ModelDescriptor>,
    status: ProviderCatalogStatus,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ModelPicker {
    open: bool,
    request_sequence: u64,
    active_request: Option<ModelListRequest>,
    /// What the Model chosen here is for, which the opener decides.
    purpose: ModelPickerPurpose,
    /// The Selection this picker was opened onto, kept because the Selection a
    /// caller passes in later is the Session's own — and a picker opened to
    /// choose a Setting's value must not be dragged onto the Model the reader
    /// happens to be conversing at when the catalog lands.
    opened_on: Option<AgentSelection>,
    provider_scope: Option<ProviderId>,
    query: String,
    cached_providers: Vec<ProviderModels>,
    providers: Vec<ProviderModels>,
    selected: Option<PickerSelection>,
    cursor_moved: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ModelPickerRow<'a> {
    Provider {
        name: &'a str,
        refreshing: bool,
    },
    Model {
        model: &'a ModelDescriptor,
        selected: bool,
        current: bool,
    },
    Error {
        name: &'a str,
        message: &'a str,
        selected: bool,
    },
    /// A Provider the user cannot use yet. It keeps its place in the list with
    /// its reason on show, its Models render unselectable, and choosing the row
    /// re-lists the catalog for a user who has just fixed the condition.
    Unavailable {
        name: &'a str,
        reason: ProviderUnavailability,
        message: &'a str,
        selected: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) enum ModelPickerAction {
    Select(ModelDescriptor),
    Retry,
}

impl ModelPicker {
    pub(super) fn cached_model_for_options(
        &self,
        current: Option<&AgentSelection>,
    ) -> Option<ModelDescriptor> {
        if let Some(current) = current {
            return self.cached_model(&current.provider, &current.model);
        }
        let mut defaults = self.cached_providers.iter().filter_map(default_model);
        let model = defaults.next()?;
        defaults.next().is_none().then(|| model.clone())
    }

    pub(super) fn cached_model(
        &self,
        provider: &ProviderId,
        model: &ModelId,
    ) -> Option<ModelDescriptor> {
        self.cached_providers
            .iter()
            .find(|catalog| &catalog.provider == provider)?
            .models
            .iter()
            .find(|descriptor| &descriptor.id == model)
            .cloned()
    }

    pub(super) fn begin_refresh(&mut self) -> ModelListRequest {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let request = ModelListRequest::new(self.request_sequence);
        self.active_request = Some(request.clone());
        request
    }

    pub(super) fn is_active_request(&self, request: &ModelListRequest) -> bool {
        self.active_request.as_ref() == Some(request)
    }

    pub(super) fn open(
        &mut self,
        current: Option<&AgentSelection>,
        provider_scope: Option<ProviderId>,
        purpose: ModelPickerPurpose,
    ) -> ModelListRequest {
        self.open = true;
        self.query.clear();
        self.purpose = purpose;
        self.opened_on = current.cloned();
        self.provider_scope = provider_scope;
        self.providers.clone_from(&self.cached_providers);
        self.cursor_moved = false;
        let request = self.begin_refresh();
        self.focus(current, false);
        request
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.purpose = ModelPickerPurpose::default();
        self.opened_on = None;
        self.selected = None;
        self.cursor_moved = false;
    }

    pub(super) fn purpose(&self) -> ModelPickerPurpose {
        self.purpose
    }

    /// The Selection this picker reads as the one in force — which row it
    /// focuses, and which row it marks as the reader's current choice.
    ///
    /// A picker choosing the Agent takes the Session's own Selection, which is
    /// what its callers hand in. A picker choosing a Setting's value takes the
    /// Selection that Setting was holding when the row opened it instead: the
    /// Session's Model is not what this picker is asking about, and letting it
    /// in would move the focus off the reader's pin the moment a catalog
    /// listing came back.
    fn selection_in_force(&self, current: Option<&AgentSelection>) -> Option<AgentSelection> {
        match self.purpose {
            ModelPickerPurpose::AgentSelection => current.cloned(),
            ModelPickerPurpose::Setting(_) => self.opened_on.clone(),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.active_request.is_some() && self.providers.is_empty()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn provider_scope(&self) -> Option<&ProviderId> {
        self.provider_scope.as_ref()
    }

    /// The Provider's name as the user reads it, from whichever catalog the
    /// picker has heard — the live list first, then the cache. A Provider no
    /// catalog has named yet goes by its wire identifier, the only name known.
    pub(super) fn provider_display_name<'a>(&'a self, provider: &'a ProviderId) -> &'a str {
        self.providers
            .iter()
            .chain(self.cached_providers.iter())
            .find(|candidate| &candidate.provider == provider)
            .map_or(provider.as_str(), |candidate| &candidate.display_name)
    }

    pub(super) fn refocus(&mut self, current: Option<&AgentSelection>) {
        if self.open {
            self.cursor_moved = false;
            let in_force = self.selection_in_force(current);
            self.focus(in_force.as_ref(), false);
        }
    }

    pub(super) fn load(
        &mut self,
        request: &ModelListRequest,
        catalog: ModelCatalog,
        current: Option<&AgentSelection>,
    ) {
        if self.active_request.as_ref() != Some(request) {
            return;
        }
        let providers = normalize_catalog(catalog.providers);
        self.cached_providers.clone_from(&providers);
        if self.providers.is_empty() {
            self.providers = providers;
        } else {
            self.merge(providers);
        }
        if let Some(scope) = self.provider_scope.as_ref()
            && !self
                .providers
                .iter()
                .any(|provider| provider.provider == *scope)
        {
            self.providers.push(ProviderModels {
                display_name: scope.to_string(),
                provider: scope.clone(),
                models: Vec::new(),
                status: ProviderCatalogStatus::Failed {
                    message: "Provider catalog is unavailable".to_owned(),
                },
            });
        }
        let in_force = self.selection_in_force(current);
        self.focus(in_force.as_ref(), self.cursor_moved);
    }

    pub(super) fn finish(&mut self, request: &ModelListRequest) {
        if self.active_request.as_ref() == Some(request) {
            self.active_request = None;
        }
    }

    pub(super) fn fail(&mut self, request: &ModelListRequest, error: String) {
        if self.active_request.as_ref() != Some(request) {
            return;
        }
        if self.providers.is_empty() {
            let provider = self
                .provider_scope
                .clone()
                .unwrap_or_else(|| ProviderId::new("catalog"));
            self.providers.push(ProviderModels {
                display_name: provider.to_string(),
                provider,
                models: Vec::new(),
                status: ProviderCatalogStatus::Failed { message: error },
            });
        } else {
            for provider in &mut self.providers {
                provider.status = ProviderCatalogStatus::Stale {
                    message: error.clone(),
                };
            }
        }
        self.active_request = None;
        self.focus(None, true);
    }

    pub(super) fn begin_retry(&mut self) -> Option<ModelListRequest> {
        matches!(self.selected, Some(PickerSelection::Retry(_))).then(|| {
            self.request_sequence = self.request_sequence.wrapping_add(1);
            let request = ModelListRequest::new(self.request_sequence);
            self.active_request = Some(request.clone());
            request
        })
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.select_first_visible();
    }

    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.select_first_visible();
    }

    pub(super) fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub(super) fn page_previous(&mut self) {
        self.move_selection(-10);
    }

    pub(super) fn page_next(&mut self) {
        self.move_selection(10);
    }

    pub(super) fn choose(&self) -> Option<ModelPickerAction> {
        match self.selected.as_ref()? {
            PickerSelection::Model(key) => self
                .providers
                .iter()
                .flat_map(|provider| &provider.models)
                .find(|model| ModelKey::from_model(model) == *key)
                .filter(|model| model.availability == ModelAvailability::Available)
                .cloned()
                .map(ModelPickerAction::Select),
            PickerSelection::Retry(_) => Some(ModelPickerAction::Retry),
        }
    }

    pub(super) fn visible_rows(
        &self,
        capacity: usize,
        current: Option<&AgentSelection>,
    ) -> impl Iterator<Item = ModelPickerRow<'_>> {
        let rows = self.rows(self.selection_in_force(current).as_ref());
        let selected = rows
            .iter()
            .position(|row| match row {
                ModelPickerRow::Model { selected, .. }
                | ModelPickerRow::Error { selected, .. }
                | ModelPickerRow::Unavailable { selected, .. } => *selected,
                ModelPickerRow::Provider { .. } => false,
            })
            .unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        rows.into_iter().skip(start).take(capacity)
    }

    pub(super) fn has_rows(&self) -> bool {
        !self.selectable().is_empty()
    }

    pub(super) fn selection_summary(&self, selection: &AgentSelection, detailed: bool) -> String {
        let Some(model) = self
            .providers
            .iter()
            .flat_map(|provider| &provider.models)
            .find(|model| model.provider == selection.provider && model.id == selection.model)
        else {
            return if detailed {
                format!(
                    "{} · {}",
                    self.provider_display_name(&selection.provider),
                    selection.model
                )
            } else {
                selection.model.to_string()
            };
        };
        if !detailed {
            return model.display_name.clone();
        }
        let mut parts = vec![
            self.provider_display_name(&selection.provider).to_owned(),
            model.display_name.clone(),
        ];
        for role in [
            ModelOptionRole::ReasoningEffort,
            ModelOptionRole::Context,
            ModelOptionRole::Speed,
        ] {
            let Some(descriptor) = model.options.iter().find(|option| option.role == role) else {
                continue;
            };
            let Some(selected) = selection
                .options
                .iter()
                .find(|selected| selected.id == descriptor.id)
            else {
                continue;
            };
            let value = match (&descriptor.kind, &selected.value) {
                (ModelOptionKind::Select { choices, .. }, ModelOptionValue::Select { choice }) => {
                    choices
                        .iter()
                        .find(|candidate| candidate.id == *choice)
                        .map_or_else(|| choice.to_string(), |choice| choice.label.clone())
                }
                (ModelOptionKind::Toggle { .. }, ModelOptionValue::Toggle { enabled }) => {
                    if *enabled { "On" } else { "Off" }.to_owned()
                }
                _ => continue,
            };
            parts.push(value);
        }
        parts.join(" · ")
    }

    fn merge(&mut self, incoming: Vec<ProviderModels>) {
        for provider in &mut self.providers {
            let Some(update) = incoming
                .iter()
                .find(|catalog| catalog.provider == provider.provider)
            else {
                for model in &mut provider.models {
                    model.availability = ModelAvailability::Unavailable;
                }
                continue;
            };
            provider.status = update.status.clone();
            provider.display_name.clone_from(&update.display_name);
            for existing in &mut provider.models {
                if let Some(model) = update.models.iter().find(|model| model.id == existing.id) {
                    *existing = model.clone();
                } else {
                    existing.availability = ModelAvailability::Unavailable;
                }
            }
            let mut additions = update
                .models
                .iter()
                .filter(|model| {
                    !provider
                        .models
                        .iter()
                        .any(|existing| existing.id == model.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            sort_models(&mut additions);
            provider.models.extend(additions);
        }
        let mut additions = incoming
            .into_iter()
            .filter(|incoming| {
                !self
                    .providers
                    .iter()
                    .any(|existing| existing.provider == incoming.provider)
            })
            .collect::<Vec<_>>();
        additions.sort_by(|left, right| left.provider.as_str().cmp(right.provider.as_str()));
        self.providers.extend(additions);
    }

    fn focus(&mut self, current: Option<&AgentSelection>, preserve_selection: bool) {
        let visible = self.selectable();
        if preserve_selection
            && self
                .selected
                .as_ref()
                .is_some_and(|selected| visible.contains(selected))
        {
            return;
        }
        if let Some(current) = current {
            let current = PickerSelection::Model(ModelKey::from_selection(current));
            if visible.contains(&current) {
                self.selected = Some(current);
                return;
            }
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| visible.contains(selected))
        {
            return;
        }
        self.selected = self
            .providers
            .iter()
            .filter(|provider| self.is_listed(provider))
            .flat_map(|provider| &provider.models)
            .find(|model| model.is_default && model_matches(&self.query, model))
            .map(|model| PickerSelection::Model(ModelKey::from_model(model)))
            .or_else(|| visible.first().cloned());
    }

    /// Whether this Provider belongs in the list at all: one the reader scoped
    /// away is somebody else's row, and one the user turned off is nobody's.
    fn is_listed(&self, provider: &ProviderModels) -> bool {
        !is_disabled(&provider.status)
            && self
                .provider_scope
                .as_ref()
                .is_none_or(|scope| *scope == provider.provider)
    }

    fn rows(&self, current: Option<&AgentSelection>) -> Vec<ModelPickerRow<'_>> {
        let current = current.map(ModelKey::from_selection);
        let mut rows = Vec::new();
        for provider in &self.providers {
            if !self.is_listed(provider) {
                continue;
            }
            let models = provider
                .models
                .iter()
                .filter(|model| model_matches(&self.query, model))
                .collect::<Vec<_>>();
            let condition = provider_condition(&provider.status);
            if models.is_empty() && condition.is_none() {
                continue;
            }
            rows.push(ModelPickerRow::Provider {
                name: &provider.display_name,
                refreshing: matches!(provider.status, ProviderCatalogStatus::Refreshing),
            });
            rows.extend(models.into_iter().map(|model| {
                let key = ModelKey::from_model(model);
                ModelPickerRow::Model {
                    model,
                    selected: self.selected == Some(PickerSelection::Model(key.clone())),
                    current: current.as_ref() == Some(&key),
                }
            }));
            if let Some(condition) = condition {
                let selected =
                    self.selected == Some(PickerSelection::Retry(provider.provider.clone()));
                rows.push(match condition {
                    ProviderCondition::Failing(message) => ModelPickerRow::Error {
                        name: &provider.display_name,
                        message,
                        selected,
                    },
                    ProviderCondition::Unavailable { reason, message } => {
                        ModelPickerRow::Unavailable {
                            name: &provider.display_name,
                            reason,
                            message,
                            selected,
                        }
                    }
                });
            }
        }
        rows
    }

    fn select_first_visible(&mut self) {
        self.selected = self.selectable().first().cloned();
        self.cursor_moved = false;
    }

    fn move_selection(&mut self, distance: isize) {
        let visible = self.selectable();
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| visible.iter().position(|candidate| candidate == selected))
            .unwrap_or(0);
        let next = (current as isize + distance).rem_euclid(visible.len() as isize) as usize;
        self.selected = Some(visible[next].clone());
        self.cursor_moved = true;
    }

    fn selectable(&self) -> Vec<PickerSelection> {
        let mut selectable = Vec::new();
        for provider in &self.providers {
            if !self.is_listed(provider) {
                continue;
            }
            selectable.extend(
                provider
                    .models
                    .iter()
                    .filter(|model| model_matches(&self.query, model))
                    .map(|model| PickerSelection::Model(ModelKey::from_model(model))),
            );
            if provider_condition(&provider.status).is_some() {
                selectable.push(PickerSelection::Retry(provider.provider.clone()));
            }
        }
        selectable
    }
}

/// The Provider's default Model, when the Provider can actually be used: a
/// Model nothing may select is no basis for the options editor's implicit
/// choice either.
fn default_model(provider: &ProviderModels) -> Option<&ModelDescriptor> {
    if is_unavailable(&provider.status) || is_disabled(&provider.status) {
        return None;
    }
    provider.models.iter().find(|model| model.is_default)
}

fn normalize_catalog(mut catalog: Vec<ProviderModelCatalog>) -> Vec<ProviderModels> {
    catalog.sort_by(|left, right| left.provider.as_str().cmp(right.provider.as_str()));
    catalog.into_iter().map(normalize_provider).collect()
}

fn normalize_provider(mut catalog: ProviderModelCatalog) -> ProviderModels {
    sort_models(&mut catalog.models);
    if is_unavailable(&catalog.status) {
        // No Model of a Provider the user cannot use can start a Session, so
        // the whole Provider renders — and refuses selection — as unavailable.
        // A disabled Provider needs nothing of the sort: Enablement is not
        // Availability, and a Provider Suru never consulted serves no Model to
        // mark either way.
        for model in &mut catalog.models {
            model.availability = ModelAvailability::Unavailable;
        }
    }
    ProviderModels {
        provider: catalog.provider,
        display_name: catalog.display_name,
        models: catalog.models,
        status: catalog.status,
    }
}

fn sort_models(models: &mut [ModelDescriptor]) {
    models.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
}

fn model_matches(query: &str, model: &ModelDescriptor) -> bool {
    fuzzy_matches(query, &model.display_name) || fuzzy_matches(query, model.id.as_str())
}

fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|character| candidate.by_ref().any(|candidate| candidate == character))
}

/// What a Provider's catalog status puts in the list beyond its Models: the
/// failure a reader may retry, or the condition making the Provider unusable.
/// Either way the row it becomes is the Provider's retry target.
#[derive(Clone, Copy, Debug)]
enum ProviderCondition<'a> {
    Failing(&'a str),
    Unavailable {
        reason: ProviderUnavailability,
        message: &'a str,
    },
}

fn provider_condition(status: &ProviderCatalogStatus) -> Option<ProviderCondition<'_>> {
    match status {
        ProviderCatalogStatus::Stale { message } | ProviderCatalogStatus::Failed { message } => {
            Some(ProviderCondition::Failing(message))
        }
        ProviderCatalogStatus::Unavailable { reason, message } => {
            Some(ProviderCondition::Unavailable {
                reason: *reason,
                message,
            })
        }
        // A Provider the user turned off puts nothing in the list — not even a
        // reason, because Suru never looked for one.
        ProviderCatalogStatus::Fresh
        | ProviderCatalogStatus::Warning { .. }
        | ProviderCatalogStatus::Refreshing
        | ProviderCatalogStatus::Disabled => None,
    }
}

fn is_unavailable(status: &ProviderCatalogStatus) -> bool {
    matches!(
        provider_condition(status),
        Some(ProviderCondition::Unavailable { .. })
    )
}

/// A Provider the user turned off leaves the list outright, rather than keeping
/// its place the way an unavailable one does: decluttering is half of what the
/// Setting is for, and a row reporting a Provider Suru never consulted would
/// replace one message with another.
fn is_disabled(status: &ProviderCatalogStatus) -> bool {
    matches!(status, ProviderCatalogStatus::Disabled)
}
