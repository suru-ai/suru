//! Searchable Model picker state and stable catalog reconciliation.

use crate::protocol::{
    AgentSelection, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionKind,
    ModelOptionRole, ModelOptionValue, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
};

use super::ModelListRequest;

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
    models: Vec<ModelDescriptor>,
    status: ProviderCatalogStatus,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ModelPicker {
    open: bool,
    request_sequence: u64,
    active_request: Option<ModelListRequest>,
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
        provider: &'a ProviderId,
        refreshing: bool,
    },
    Model {
        model: &'a ModelDescriptor,
        selected: bool,
        current: bool,
    },
    Error {
        provider: &'a ProviderId,
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
        self.cached_providers
            .iter()
            .find(|provider| provider.provider.as_str() == "codex")
            .and_then(default_model)
            .or_else(|| self.cached_providers.iter().find_map(default_model))
            .cloned()
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
    ) -> ModelListRequest {
        self.open = true;
        self.query.clear();
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
        self.selected = None;
        self.cursor_moved = false;
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

    pub(super) fn refocus(&mut self, current: Option<&AgentSelection>) {
        if self.open {
            self.cursor_moved = false;
            self.focus(current, false);
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
                provider: scope.clone(),
                models: Vec::new(),
                status: ProviderCatalogStatus::Failed {
                    message: "Provider catalog is unavailable".to_owned(),
                },
            });
        }
        self.focus(current, self.cursor_moved);
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
        let rows = self.rows(current);
        let selected =
            rows.iter()
                .position(|row| match row {
                    ModelPickerRow::Model { selected, .. }
                    | ModelPickerRow::Error { selected, .. } => *selected,
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
                    "Model {} · Provider {}",
                    selection.model, selection.provider
                )
            } else {
                format!("Model {}", selection.model)
            };
        };
        let mut parts = vec![format!("Model {}", model.display_name)];
        if !detailed {
            return parts.pop().expect("Model summary always has one part");
        }
        parts.push(format!("Provider {}", selection.provider));
        for descriptor in &model.options {
            let Some(selected) = selection
                .options
                .iter()
                .find(|selected| selected.id == descriptor.id)
            else {
                continue;
            };
            let visible = descriptor.role == ModelOptionRole::ReasoningEffort
                || matches!(
                    descriptor.role,
                    ModelOptionRole::Speed | ModelOptionRole::Context
                ) && !option_is_default(&descriptor.kind, &selected.value);
            if !visible {
                continue;
            }
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
            parts.push(format!("{} {value}", descriptor.label));
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
            .filter(|provider| {
                self.provider_scope
                    .as_ref()
                    .is_none_or(|scope| *scope == provider.provider)
            })
            .flat_map(|provider| &provider.models)
            .find(|model| model.is_default && model_matches(&self.query, model))
            .map(|model| PickerSelection::Model(ModelKey::from_model(model)))
            .or_else(|| visible.first().cloned());
    }

    fn rows(&self, current: Option<&AgentSelection>) -> Vec<ModelPickerRow<'_>> {
        let current = current.map(ModelKey::from_selection);
        let mut rows = Vec::new();
        for provider in &self.providers {
            if self
                .provider_scope
                .as_ref()
                .is_some_and(|scope| *scope != provider.provider)
            {
                continue;
            }
            let models = provider
                .models
                .iter()
                .filter(|model| model_matches(&self.query, model))
                .collect::<Vec<_>>();
            let error = provider_error(&provider.status);
            if models.is_empty() && error.is_none() {
                continue;
            }
            rows.push(ModelPickerRow::Provider {
                provider: &provider.provider,
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
            if let Some(message) = error {
                rows.push(ModelPickerRow::Error {
                    provider: &provider.provider,
                    message,
                    selected: self.selected
                        == Some(PickerSelection::Retry(provider.provider.clone())),
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
            if self
                .provider_scope
                .as_ref()
                .is_some_and(|scope| *scope != provider.provider)
            {
                continue;
            }
            selectable.extend(
                provider
                    .models
                    .iter()
                    .filter(|model| model_matches(&self.query, model))
                    .map(|model| PickerSelection::Model(ModelKey::from_model(model))),
            );
            if provider_error(&provider.status).is_some() {
                selectable.push(PickerSelection::Retry(provider.provider.clone()));
            }
        }
        selectable
    }
}

fn default_model(provider: &ProviderModels) -> Option<&ModelDescriptor> {
    provider.models.iter().find(|model| model.is_default)
}

fn normalize_catalog(mut catalog: Vec<ProviderModelCatalog>) -> Vec<ProviderModels> {
    catalog.sort_by(|left, right| left.provider.as_str().cmp(right.provider.as_str()));
    catalog.into_iter().map(normalize_provider).collect()
}

fn normalize_provider(mut catalog: ProviderModelCatalog) -> ProviderModels {
    sort_models(&mut catalog.models);
    ProviderModels {
        provider: catalog.provider,
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

fn provider_error(status: &ProviderCatalogStatus) -> Option<&str> {
    match status {
        ProviderCatalogStatus::Stale { message } | ProviderCatalogStatus::Failed { message } => {
            Some(message)
        }
        ProviderCatalogStatus::Fresh | ProviderCatalogStatus::Refreshing => None,
    }
}

fn option_is_default(kind: &ModelOptionKind, value: &ModelOptionValue) -> bool {
    match (kind, value) {
        (ModelOptionKind::Select { default, .. }, ModelOptionValue::Select { choice }) => {
            choice == default
        }
        (ModelOptionKind::Toggle { default }, ModelOptionValue::Toggle { enabled }) => {
            enabled == default
        }
        _ => false,
    }
}
