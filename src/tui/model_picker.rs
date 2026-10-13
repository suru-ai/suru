//! The Model picker: the Agent Selection list in an overlay of its own, the
//! catalog listings it asks for, and the latest catalog heard for each Outlook.

use std::collections::HashMap;

use crate::{
    protocol::{
        AgentSelection, ModelCatalog, ModelDescriptor, ModelId, ModelOptionKind, ModelOptionRole,
        ModelOptionValue, Outlook, ProviderCatalogStatus, ProviderId, SettingMutation,
    },
    theme::Theme,
};

use super::{
    ModelListRequest,
    agent_selection_list::{
        AgentSelectionAction, AgentSelectionLines, AgentSelectionList, InForce, ProviderModels,
        default_model, normalize_catalog,
    },
};

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

/// The presentation metadata known when an Outlook's current Agent Selection
/// became the one in force. It is the fallback a summary uses while no catalog
/// the picker holds names the Model — after an Outlook turn clears the caches,
/// most of all. Whenever a catalog does name it, the catalog wins, so a Model
/// is always called what its Provider calls it now.
#[derive(Clone, Debug)]
struct SelectionPresentation {
    outlook: Outlook,
    selection: AgentSelection,
    provider_display_name: String,
    model: Option<ModelDescriptor>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ModelPicker {
    outlook: Outlook,
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
    /// The latest catalog heard for each Outlook, whether a listing this
    /// picker asked for or one the server pushed. Kept per Outlook so turning
    /// back to one names its Models again without waiting to hear the catalog
    /// anew.
    cached_providers: HashMap<Outlook, Vec<ProviderModels>>,
    /// How many catalogs each Outlook's Server has pushed, which tells a
    /// reading asked of it earlier whether a newer word has landed since.
    pushes: HashMap<Outlook, u64>,
    /// The Models on show, which outlive the overlay so a Provider the picker
    /// last listed goes on being named by it.
    list: AgentSelectionList,
    /// One current presentation per Outlook, retained while another Outlook is
    /// visible so returning to it restores the label it previously showed.
    selection_presentations: Vec<SelectionPresentation>,
}

/// How the latest catalog heard for each Outlook names its Providers and
/// Models, read wherever a Session's Agent Selection is named beyond the
/// picker. A name no catalog in hand gives is nothing, for the caller to
/// stand something in for.
#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct CatalogNames<'a>(&'a HashMap<Outlook, Vec<ProviderModels>>);

impl<'a> CatalogNames<'a> {
    /// What `outlook`'s catalog calls `provider`.
    pub(in crate::tui) fn provider(
        &self,
        outlook: &Outlook,
        provider: &ProviderId,
    ) -> Option<&'a str> {
        self.0
            .get(outlook)?
            .iter()
            .find(|candidate| &candidate.provider == provider)
            .map(|candidate| candidate.display_name.as_str())
    }

    /// What `outlook`'s catalog calls `model`: as `provider` offers it where
    /// the Provider is known, and otherwise as the first Provider offering a
    /// Model by that identifier does.
    pub(in crate::tui) fn model(
        &self,
        outlook: &Outlook,
        provider: Option<&ProviderId>,
        model: &ModelId,
    ) -> Option<&'a str> {
        self.0
            .get(outlook)?
            .iter()
            .filter(|candidate| provider.is_none_or(|provider| &candidate.provider == provider))
            .flat_map(|candidate| &candidate.models)
            .find(|descriptor| &descriptor.id == model)
            .map(|descriptor| descriptor.display_name.as_str())
    }
}

/// How the latest catalog heard from one Outlook's Server stands for each of
/// its Providers, read by a surface reporting Availability beyond the picker.
/// A Provider no catalog in hand names has no status yet.
#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct CatalogStatuses<'a>(&'a [ProviderModels]);

impl<'a> CatalogStatuses<'a> {
    pub(in crate::tui) fn status(
        &self,
        provider: &ProviderId,
    ) -> Option<&'a ProviderCatalogStatus> {
        self.0
            .iter()
            .find(|candidate| &candidate.provider == provider)
            .map(|candidate| &candidate.status)
    }
}

impl ModelPicker {
    pub(super) fn cached_model_for_options(
        &self,
        current: Option<&AgentSelection>,
    ) -> Option<ModelDescriptor> {
        if let Some(current) = current {
            return self.cached_model(&current.provider, &current.model);
        }
        let mut defaults = self.cache().iter().filter_map(default_model);
        let model = defaults.next()?;
        defaults.next().is_none().then(|| model.clone())
    }

    pub(super) fn cached_model(
        &self,
        provider: &ProviderId,
        model: &ModelId,
    ) -> Option<ModelDescriptor> {
        self.cache()
            .iter()
            .find(|catalog| &catalog.provider == provider)?
            .models
            .iter()
            .find(|descriptor| &descriptor.id == model)
            .cloned()
    }

    pub(super) fn begin_refresh(&mut self) -> ModelListRequest {
        self.begin_refresh_of(self.outlook.clone())
    }

    /// Asks for `outlook`'s catalog anew, whichever Outlook is on screen, as
    /// Onboarding does of the Client's own Server. A listing of another
    /// Outlook than the one on screen feeds that Outlook's cache alone.
    pub(super) fn begin_refresh_of(&mut self, outlook: Outlook) -> ModelListRequest {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let request = ModelListRequest::new(self.request_sequence, outlook);
        self.active_request = Some(request.clone());
        request
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        if self.outlook == outlook {
            return;
        }
        self.outlook = outlook;
        self.close();
        self.active_request = None;
        self.list.clear();
    }

    /// The latest catalog heard for the current Outlook, or nothing yet.
    fn cache(&self) -> &[ProviderModels] {
        self.catalog_of(&self.outlook)
    }

    /// The latest catalog heard for `outlook`, or nothing yet.
    pub(super) fn catalog_of(&self, outlook: &Outlook) -> &[ProviderModels] {
        self.cached_providers
            .get(outlook)
            .map_or(&[], Vec::as_slice)
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
        self.purpose = purpose;
        self.opened_on = current.cloned();
        self.list.open(
            self.cache().to_vec(),
            provider_scope,
            current.map(InForce::Model),
        );
        self.begin_refresh()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.purpose = ModelPickerPurpose::default();
        self.opened_on = None;
        self.list.close();
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
    pub(super) fn selection_in_force(
        &self,
        current: Option<&AgentSelection>,
    ) -> Option<AgentSelection> {
        match self.purpose {
            ModelPickerPurpose::AgentSelection => current.cloned(),
            ModelPickerPurpose::Setting(_) => self.opened_on.clone(),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.active_request.is_some() && self.list.is_empty()
    }

    pub(super) fn query(&self) -> &str {
        self.list.query()
    }

    pub(super) fn provider_scope(&self) -> Option<&ProviderId> {
        self.list.provider_scope()
    }

    /// The Provider's name as the user reads it, from whichever catalog the
    /// picker has heard — the live list first, then the cache. A Provider no
    /// catalog has named yet goes by its wire identifier, the only name known.
    pub(super) fn provider_display_name<'a>(&'a self, provider: &'a ProviderId) -> &'a str {
        self.listed_provider(provider)
            .map_or(provider.as_str(), |candidate| &candidate.display_name)
    }

    /// How the latest catalog heard for each Outlook names its Providers and
    /// Models, for a surface naming them outside the picker.
    pub(super) fn catalog_names(&self) -> CatalogNames<'_> {
        CatalogNames(&self.cached_providers)
    }

    /// How the latest catalog heard for `outlook` stands for each Provider,
    /// for a surface reporting Availability outside the picker.
    pub(super) fn catalog_statuses(&self, outlook: &Outlook) -> CatalogStatuses<'_> {
        CatalogStatuses(self.catalog_of(outlook))
    }

    /// The Provider as the latest catalog lists it: the live list first, then
    /// the current Outlook's cache. Nothing while no catalog has named it.
    fn listed_provider(&self, provider: &ProviderId) -> Option<&ProviderModels> {
        self.list.provider(provider).or_else(|| {
            self.cache()
                .iter()
                .find(|candidate| &candidate.provider == provider)
        })
    }

    /// Puts focus back on the Selection in force, which another client
    /// changing it asks for. The window follows focus there as the keys
    /// would carry it.
    pub(super) fn refocus(&mut self, current: Option<&AgentSelection>) {
        if self.open {
            let in_force = self.selection_in_force(current);
            self.list.refocus(in_force.as_ref().map(InForce::Model));
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
        self.cached_providers
            .insert(request.outlook().clone(), providers.clone());
        if request.outlook() != &self.outlook {
            return;
        }
        let in_force = self.selection_in_force(current);
        self.list
            .load(providers, in_force.as_ref().map(InForce::Model));
    }

    /// Takes in a catalog a server pushed unasked, for the Outlook it serves.
    /// It feeds the cache every summary and Provider name is read from, and
    /// nothing else: the rows of a picker already open keep answering to the
    /// listing it asked for.
    pub(super) fn adopt_catalog(&mut self, outlook: Outlook, catalog: ModelCatalog) {
        *self.pushes.entry(outlook.clone()).or_default() += 1;
        self.cached_providers
            .insert(outlook, normalize_catalog(catalog.providers));
    }

    /// How many catalogs `outlook`'s Server has pushed so far.
    pub(super) fn pushes(&self, outlook: &Outlook) -> u64 {
        self.pushes.get(outlook).copied().unwrap_or_default()
    }

    /// Takes in what a reading of `outlook`'s Server found, asked of it when
    /// it had pushed `pushes` catalogs, into the cache as a push would go —
    /// unless it has pushed one since, which is its newer word.
    pub(super) fn adopt_reading(&mut self, outlook: Outlook, catalog: ModelCatalog, pushes: u64) {
        if self.pushes(&outlook) == pushes {
            self.cached_providers
                .insert(outlook, normalize_catalog(catalog.providers));
        }
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
        if request.outlook() == &self.outlook {
            self.list.fail(error);
        }
        self.active_request = None;
    }

    pub(super) fn begin_retry(&mut self) -> Option<ModelListRequest> {
        self.list.is_retry_selected().then(|| {
            self.request_sequence = self.request_sequence.wrapping_add(1);
            let request = ModelListRequest::new(self.request_sequence, self.outlook.clone());
            self.active_request = Some(request.clone());
            request
        })
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.list.insert(text);
    }

    pub(super) fn delete_backward(&mut self) {
        self.list.delete_backward();
    }

    pub(super) fn select_previous(&mut self) {
        self.list.select_previous();
    }

    pub(super) fn select_next(&mut self) {
        self.list.select_next();
    }

    pub(super) fn page_previous(&mut self) {
        self.list.page_previous();
    }

    pub(super) fn page_next(&mut self) {
        self.list.page_next();
    }

    pub(super) fn choose(&self) -> Option<AgentSelectionAction> {
        self.list.choose()
    }

    /// The Models the window shows, drawn in `capacity` lines of a list
    /// `width` columns wide with the Selection in force marked; see
    /// [`AgentSelectionList::draw`].
    pub(super) fn draw_rows(
        &self,
        capacity: usize,
        height: usize,
        width: u16,
        current: Option<&AgentSelection>,
        theme: &Theme,
    ) -> AgentSelectionLines<'_> {
        let in_force = self.selection_in_force(current);
        self.list.draw(
            capacity,
            height,
            width,
            in_force.as_ref().map(InForce::Model),
            theme,
        )
    }

    pub(super) fn has_rows(&self) -> bool {
        self.list.has_rows()
    }

    pub(super) fn selection_summary(&self, selection: &AgentSelection, detailed: bool) -> String {
        let presentation = self.selection_presentations.iter().find(|presentation| {
            presentation.outlook == self.outlook && presentation.selection == *selection
        });
        // The latest catalog names the Model; the presentation frozen when the
        // selection came into force stands in only while no catalog does.
        let listed = self.listed_provider(&selection.provider);
        let provider_display_name = listed
            .map(|provider| provider.display_name.as_str())
            .or_else(|| {
                presentation.map(|presentation| presentation.provider_display_name.as_str())
            })
            .unwrap_or(selection.provider.as_str());
        let model = listed
            .and_then(|provider| {
                provider
                    .models
                    .iter()
                    .find(|model| model.id == selection.model)
            })
            .or_else(|| presentation.and_then(|presentation| presentation.model.as_ref()));
        let Some(model) = model else {
            return if detailed {
                format!("{provider_display_name} · {}", selection.model)
            } else {
                selection.model.to_string()
            };
        };
        if !detailed {
            return model.display_name.clone();
        }
        let mut parts = vec![provider_display_name.to_owned(), model.display_name.clone()];
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

    /// Keeps the best presentation currently available beside the durable
    /// selection, as the fallback a summary reads when no catalog the picker
    /// holds names the Model. A missing catalog has an explicit ID fallback. A
    /// real selection change replaces the Outlook's presentation, and an
    /// interval with no selection ends it so reselecting the same Agent can
    /// capture metadata learned in the meantime.
    pub(super) fn remember_selection(&mut self, selection: Option<&AgentSelection>) {
        let Some(selection) = selection else {
            let outlook = self.outlook.clone();
            self.selection_presentations
                .retain(|presentation| presentation.outlook != outlook);
            return;
        };
        let existing = self
            .selection_presentations
            .iter()
            .position(|presentation| presentation.outlook == self.outlook);
        if existing.is_some_and(|index| self.selection_presentations[index].selection == *selection)
        {
            return;
        }
        let provider = self.listed_provider(&selection.provider);
        let provider_display_name = provider.map_or_else(
            || selection.provider.to_string(),
            |provider| provider.display_name.clone(),
        );
        let model = provider.and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| model.id == selection.model)
                .cloned()
        });
        let presentation = SelectionPresentation {
            outlook: self.outlook.clone(),
            selection: selection.clone(),
            provider_display_name,
            model,
        };
        if let Some(index) = existing {
            self.selection_presentations[index] = presentation;
        } else {
            self.selection_presentations.push(presentation);
        }
    }
}
