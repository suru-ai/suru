//! The list of Models a reader chooses an Agent Selection from: each Provider's
//! heading over its Models, beside the retry row of a Provider that failed to
//! list them or that the user cannot use yet, beneath whatever rows its host
//! names above them. It is one list wherever it is hosted — the Model picker
//! draws it inside its own chrome, and Onboarding's Derivation step beneath
//! the derivation Setting's named values — so it keeps its rows, what narrows
//! them, which row focus stands on, and the window it is read through, and
//! draws its rows, leaving the box around them, and whatever the reader's
//! choice is for, to its host.

use ratatui::{style::Modifier, text::Line};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{
        AgentSelection, ModelAvailability, ModelDescriptor, ModelId, ProviderCatalogStatus,
        ProviderId, ProviderModelCatalog, ProviderUnavailability,
    },
    theme::Theme,
};

use super::{
    fuzzy::fuzzy_matches,
    list_window::{ListWindow, WindowEntry},
    selection::WrappedLine,
    slots::truncate_to_width,
    wrapped_text::{FIX_INDENT, WrappedText},
};

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
enum ListSelection {
    Named(usize),
    Model(ModelKey),
    Retry(ProviderId),
}

/// The choice a list reads as the one in force: the row it marks as the
/// reader's current choice, and the row focus goes to wherever the reader has
/// not moved it.
#[derive(Clone, Copy, Debug)]
pub(super) enum InForce<'a> {
    /// One of the rows the host named above the Providers, by its place
    /// among them.
    Named(usize),
    /// The Model a Selection names.
    Model(&'a AgentSelection),
}

impl InForce<'_> {
    fn selection(self) -> ListSelection {
        match self {
            Self::Named(index) => ListSelection::Named(index),
            Self::Model(selection) => ListSelection::Model(ModelKey::from_selection(selection)),
        }
    }
}

/// One Provider as a catalog lists it: its name, its Models in the order the
/// list draws them, and how its catalog stands.
#[derive(Clone, Debug)]
pub(super) struct ProviderModels {
    pub(super) provider: ProviderId,
    /// The Provider's name as the user reads it. Catalog-served rows carry the
    /// runtime-declared name; rows the list invents for a Provider no catalog
    /// answered for fall back to the wire identifier, the only name it has.
    pub(super) display_name: String,
    pub(super) models: Vec<ModelDescriptor>,
    pub(super) status: ProviderCatalogStatus,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AgentSelectionList {
    /// The rows the host names above the Providers, each a choice that is no
    /// Model. They stand whatever the query, being few and offered first.
    named: Vec<&'static str>,
    providers: Vec<ProviderModels>,
    /// The one Provider whose Models are listed, where the host narrowed the
    /// list to it.
    provider_scope: Option<ProviderId>,
    query: String,
    selected: Option<ListSelection>,
    cursor_moved: bool,
    window: ListWindow,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum AgentSelectionRow<'a> {
    Named {
        label: &'a str,
        selected: bool,
        current: bool,
    },
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
    /// its reason on show and what fixes it beneath, its Models render
    /// unselectable, and choosing the row re-lists the catalog for a user who
    /// has just fixed the condition.
    Unavailable {
        name: &'a str,
        reason: ProviderUnavailability,
        message: &'a str,
        fix: &'a str,
        selected: bool,
    },
}

/// The reader's keys on an Agent Selection list: they move Row Focus and edit
/// the query narrowing it, and nothing more, so whatever hosts the list can
/// lend them out without anything listing other rows behind its back.
pub(super) struct ListKeys<'a>(&'a mut AgentSelectionList);

impl ListKeys<'_> {
    pub(super) fn insert(self, text: &str) {
        self.0.query.push_str(text);
        self.0.select_first_visible();
    }

    pub(super) fn delete_backward(self) {
        self.0.query.pop();
        self.0.select_first_visible();
    }

    pub(super) fn select_previous(self) {
        self.0.move_selection(-1);
    }

    pub(super) fn select_next(self) {
        self.0.move_selection(1);
    }

    pub(super) fn page_previous(self) {
        self.0.move_selection(-10);
    }

    pub(super) fn page_next(self) {
        self.0.move_selection(10);
    }
}

impl AgentSelectionRow<'_> {
    fn is_selected(&self) -> bool {
        match self {
            Self::Named { selected, .. }
            | Self::Model { selected, .. }
            | Self::Error { selected, .. }
            | Self::Unavailable { selected, .. } => *selected,
            Self::Provider { .. } => false,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum AgentSelectionAction {
    /// One of the rows the host named, by its place among them.
    Named(usize),
    Select(ModelDescriptor),
    Retry,
}

/// The rows a list drew, for its host to place beneath whatever it draws
/// above them.
pub(super) struct AgentSelectionLines<'a> {
    pub(super) lines: Vec<Line<'static>>,
    /// Each fix drawn beneath its retry row, placed by the line of `lines` it
    /// begins on.
    pub(super) wrapped: Vec<WrappedLine<'a>>,
}

impl AgentSelectionList {
    /// A list offering `named` rows, in that order, above every Provider.
    pub(super) fn with_named(named: Vec<&'static str>) -> Self {
        Self {
            named,
            ..Self::default()
        }
    }

    /// Lists `providers` afresh, narrowed to `provider_scope` where there is
    /// one, with an empty query and focus on `current` — the list opening at
    /// its head and carried to focus from there.
    pub(super) fn open(
        &mut self,
        providers: Vec<ProviderModels>,
        provider_scope: Option<ProviderId>,
        current: Option<InForce<'_>>,
    ) {
        self.query.clear();
        self.provider_scope = provider_scope;
        self.providers = providers;
        self.cursor_moved = false;
        self.window.open();
        self.focus(current, false);
    }

    /// Lets go of the query and focus; the rows stay, so a Provider the list
    /// last named goes on being named by it.
    pub(super) fn close(&mut self) {
        self.query.clear();
        self.selected = None;
        self.cursor_moved = false;
    }

    /// Forgets the rows and the scope along with them, for a list that will
    /// next be about another Outlook.
    pub(super) fn clear(&mut self) {
        self.providers.clear();
        self.provider_scope = None;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn provider_scope(&self) -> Option<&ProviderId> {
        self.provider_scope.as_ref()
    }

    /// The Provider as the rows list it, while they do.
    pub(super) fn provider(&self, provider: &ProviderId) -> Option<&ProviderModels> {
        self.providers
            .iter()
            .find(|candidate| &candidate.provider == provider)
    }

    /// Takes focus where it stands as the reader's own, as moving it there
    /// would: a host whose reader chose the row focus stands on keeps it
    /// there however the rows or the choice in force change.
    pub(super) fn hold_focus(&mut self) {
        self.cursor_moved = true;
    }

    /// Puts focus back on `current`, and the window follows focus there as
    /// the keys would carry it.
    pub(super) fn refocus(&mut self, current: Option<InForce<'_>>) {
        self.cursor_moved = false;
        self.focus(current, false);
        self.window.reveal();
    }

    /// Takes in the rows a listing answered with: in place of the rows on
    /// show while there are none, and otherwise merged into them, so the rows
    /// a reader is looking at stay where they are. Focus stays where the
    /// reader moved it, and otherwise goes to `current`.
    pub(super) fn load(&mut self, providers: Vec<ProviderModels>, current: Option<InForce<'_>>) {
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
        self.focus(current, self.cursor_moved);
    }

    /// Lists `providers` in place of the rows on show, for a host whose rows
    /// follow every catalog it hears rather than one listing it asked for.
    /// Focus stays where the reader moved it while that row is still listed,
    /// and otherwise goes to `current`.
    pub(super) fn relist(&mut self, providers: Vec<ProviderModels>, current: Option<InForce<'_>>) {
        let (focused, from) = (self.selected.clone(), self.focus_entry());
        self.providers = providers;
        self.focus(current, self.cursor_moved);
        self.follow_focus(focused, from);
    }

    /// Takes in a listing that failed: a row a reader may retry where there
    /// were none, and otherwise every Provider's rows marked as what was last
    /// heard.
    pub(super) fn fail(&mut self, error: &str) {
        let (focused, from) = (self.selected.clone(), self.focus_entry());
        let providers = std::mem::take(&mut self.providers);
        self.providers = failed_listing(providers, self.provider_scope.as_ref(), error);
        self.focus(None, true);
        self.follow_focus(focused, from);
    }

    /// Brings the window along once the rows changed under focus, which
    /// stood on `focused`, at entry `from`, before they did.
    fn follow_focus(&self, focused: Option<ListSelection>, from: Option<usize>) {
        if self.selected != focused {
            // Focus carried to another row comes into view as the keys would
            // have carried it there.
            self.window.reveal();
        } else if let (Some(from), Some(to)) = (from, self.focus_entry())
            && from != to
        {
            // Rows listed or taken away above focus move no window while
            // focus stays in view, and only as far as keeps it in view once
            // it would not.
            self.window.shift_focus(from, to);
        }
    }

    /// Whether focus stands on a row that re-lists the catalog.
    pub(super) fn is_retry_selected(&self) -> bool {
        matches!(self.selected, Some(ListSelection::Retry(_)))
    }

    /// What the reader's keys may do to the list.
    pub(super) fn keys(&mut self) -> ListKeys<'_> {
        ListKeys(self)
    }

    pub(super) fn choose(&self) -> Option<AgentSelectionAction> {
        match self.selected.as_ref()? {
            ListSelection::Named(index) => Some(AgentSelectionAction::Named(*index)),
            ListSelection::Model(key) => self
                .providers
                .iter()
                .flat_map(|provider| &provider.models)
                .find(|model| ModelKey::from_model(model) == *key)
                .filter(|model| model.availability == ModelAvailability::Available)
                .cloned()
                .map(AgentSelectionAction::Select),
            ListSelection::Retry(_) => Some(AgentSelectionAction::Retry),
        }
    }

    /// The rows the window shows in `capacity` lines of a list `width`
    /// columns wide, which decides how many lines each fix beneath a retry
    /// row takes.
    fn visible_rows(
        &self,
        capacity: usize,
        width: u16,
        current: Option<InForce<'_>>,
    ) -> impl Iterator<Item = AgentSelectionRow<'_>> {
        let rows = self.rows(current);
        let selected = rows.iter().position(AgentSelectionRow::is_selected);
        // A Provider's heading is a Row the keys step over, so it comes into
        // view with the Models around it rather than counting as one of them.
        let entries = rows
            .iter()
            .map(|row| match row {
                AgentSelectionRow::Provider { .. } => WindowEntry::passive(1),
                AgentSelectionRow::Named { .. }
                | AgentSelectionRow::Model { .. }
                | AgentSelectionRow::Error { .. } => WindowEntry::ROW,
                AgentSelectionRow::Unavailable { fix, .. } => {
                    WindowEntry::focusable(1 + WrappedText::new(fix, FIX_INDENT, width).height())
                }
            })
            .collect::<Vec<_>>();
        let shown = self.window.settle(&entries, capacity, selected);
        rows.into_iter().skip(shown.start).take(shown.len())
    }

    /// The rows the window shows, drawn in `capacity` lines of a list `width`
    /// columns wide, with `current` marked as the choice in force. An entry
    /// taller than the `height` lines left to draw in is drawn from its end.
    pub(super) fn draw(
        &self,
        capacity: usize,
        height: usize,
        width: u16,
        current: Option<InForce<'_>>,
        theme: &Theme,
    ) -> AgentSelectionLines<'_> {
        let mut lines = Vec::new();
        let mut wrapped = Vec::new();
        for row in self.visible_rows(capacity, width, current) {
            match row {
                AgentSelectionRow::Named {
                    label,
                    selected,
                    current,
                } => lines.push(Line::styled(
                    choice_row_text(
                        label,
                        None,
                        &current_state(current),
                        selected,
                        usize::from(width),
                    ),
                    if selected {
                        theme.selection.focused
                    } else {
                        theme.text.primary
                    },
                )),
                AgentSelectionRow::Provider { name, refreshing } => lines.push(Line::styled(
                    truncate_to_width(
                        &format!("{name}{}", if refreshing { " · refreshing" } else { "" }),
                        usize::from(width),
                    ),
                    theme.accent.primary.add_modifier(Modifier::BOLD),
                )),
                AgentSelectionRow::Model {
                    model,
                    selected,
                    current,
                } => lines.push(Line::styled(
                    model_row_text(model, selected, current, usize::from(width)),
                    if selected {
                        theme.selection.focused
                    } else if model.availability == ModelAvailability::Unavailable {
                        theme.text.subdued
                    } else {
                        theme.text.primary
                    },
                )),
                // A failing Provider and an unavailable one are the same row:
                // the retry the reader may take, with what it is about to
                // re-check. Only the account of the condition differs, and an
                // unavailable one says beneath it what the reader does to fix
                // it, on lines of its own so none of it is cut short — and so
                // that a selection across them copies the fix as written.
                AgentSelectionRow::Error {
                    name,
                    message,
                    selected,
                } => lines.push(retry_row(
                    &format!("Retry {name}: {message}"),
                    selected,
                    width,
                    theme,
                )),
                AgentSelectionRow::Unavailable {
                    name,
                    reason,
                    message,
                    fix,
                    selected,
                } => {
                    lines.push(retry_row(
                        &format!("Retry {name}: {} · {message}", reason.label()),
                        selected,
                        width,
                        theme,
                    ));
                    let style = if selected {
                        theme.selection.focused
                    } else {
                        theme.text.primary
                    };
                    // An entry taller than the list has rows for is drawn
                    // from its end: the retry row, then as much of the fix's
                    // end as fits, which is where every fix names the page or
                    // command it sends the reader to.
                    let room = height.saturating_sub(lines.len());
                    let fix = WrappedText::new(fix, FIX_INDENT, width).last(room);
                    wrapped.push(fix.wrapped(lines.len()));
                    lines.extend(fix.lines(style));
                }
            }
        }
        AgentSelectionLines { lines, wrapped }
    }

    /// Where focus stands among the entries the window counts, Provider
    /// headings included.
    fn focus_entry(&self) -> Option<usize> {
        self.rows(None)
            .iter()
            .position(AgentSelectionRow::is_selected)
    }

    pub(super) fn has_rows(&self) -> bool {
        !self.selectable().is_empty()
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

    fn focus(&mut self, current: Option<InForce<'_>>, preserve_selection: bool) {
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
            let current = current.selection();
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
        // A host naming rows of its own offers them first, so focus with
        // nothing else to stand on stands on the first of them rather than on
        // some Provider's default Model.
        let default_model = self
            .providers
            .iter()
            .filter(|provider| self.named.is_empty() && self.is_listed(provider))
            .flat_map(|provider| &provider.models)
            .find(|model| model.is_default && model_matches(&self.query, model))
            .map(|model| ListSelection::Model(ModelKey::from_model(model)));
        self.selected = default_model.or_else(|| visible.first().cloned());
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

    fn rows(&self, current: Option<InForce<'_>>) -> Vec<AgentSelectionRow<'_>> {
        let current = current.map(InForce::selection);
        let mut rows = self
            .named
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let named = Some(ListSelection::Named(index));
                AgentSelectionRow::Named {
                    label,
                    selected: self.selected == named,
                    current: current == named,
                }
            })
            .collect::<Vec<_>>();
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
            rows.push(AgentSelectionRow::Provider {
                name: &provider.display_name,
                refreshing: matches!(provider.status, ProviderCatalogStatus::Refreshing),
            });
            rows.extend(models.into_iter().map(|model| {
                let row = Some(ListSelection::Model(ModelKey::from_model(model)));
                AgentSelectionRow::Model {
                    model,
                    selected: self.selected == row,
                    current: current == row,
                }
            }));
            if let Some(condition) = condition {
                let selected =
                    self.selected == Some(ListSelection::Retry(provider.provider.clone()));
                rows.push(match condition {
                    ProviderCondition::Failing(message) => AgentSelectionRow::Error {
                        name: &provider.display_name,
                        message,
                        selected,
                    },
                    ProviderCondition::Unavailable {
                        reason,
                        message,
                        fix,
                    } => AgentSelectionRow::Unavailable {
                        name: &provider.display_name,
                        reason,
                        message,
                        fix,
                        selected,
                    },
                });
            }
        }
        rows
    }

    /// Puts focus on the first Model a new query leaves, which makes the
    /// results another list: it opens at its head.
    fn select_first_visible(&mut self) {
        self.selected = self.selectable().first().cloned();
        self.cursor_moved = false;
        self.window.open();
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
        self.window.reveal();
    }

    fn selectable(&self) -> Vec<ListSelection> {
        let mut selectable = (0..self.named.len())
            .map(ListSelection::Named)
            .collect::<Vec<_>>();
        for provider in &self.providers {
            if !self.is_listed(provider) {
                continue;
            }
            selectable.extend(
                provider
                    .models
                    .iter()
                    .filter(|model| model_matches(&self.query, model))
                    .map(|model| ListSelection::Model(ModelKey::from_model(model))),
            );
            if provider_condition(&provider.status).is_some() {
                selectable.push(ListSelection::Retry(provider.provider.clone()));
            }
        }
        selectable
    }
}

/// `providers` as a listing that failed with `error` leaves them: every one
/// marked as what was last heard, each with a row to retry beneath it, or —
/// where there were none — one row for `scope`, or the whole catalog, to
/// retry.
pub(super) fn failed_listing(
    mut providers: Vec<ProviderModels>,
    scope: Option<&ProviderId>,
    error: &str,
) -> Vec<ProviderModels> {
    if providers.is_empty() {
        let provider = scope.cloned().unwrap_or_else(|| ProviderId::new("catalog"));
        providers.push(ProviderModels {
            display_name: provider.to_string(),
            provider,
            models: Vec::new(),
            status: ProviderCatalogStatus::Failed {
                message: error.to_owned(),
            },
        });
    } else {
        for provider in &mut providers {
            provider.status = ProviderCatalogStatus::Stale {
                message: error.to_owned(),
            };
        }
    }
    providers
}

/// The Provider's default Model, when the Provider can actually be used: a
/// Model nothing may select is no basis for the options editor's implicit
/// choice either.
pub(super) fn default_model(provider: &ProviderModels) -> Option<&ModelDescriptor> {
    if is_unavailable(&provider.status) || is_disabled(&provider.status) {
        return None;
    }
    provider.models.iter().find(|model| model.is_default)
}

pub(super) fn normalize_catalog(mut catalog: Vec<ProviderModelCatalog>) -> Vec<ProviderModels> {
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

/// What a Provider's catalog status puts in the list beyond its Models: the
/// failure a reader may retry, or the condition making the Provider unusable.
/// Either way the row it becomes is the Provider's retry target.
#[derive(Clone, Copy, Debug)]
enum ProviderCondition<'a> {
    Failing(&'a str),
    Unavailable {
        reason: ProviderUnavailability,
        message: &'a str,
        fix: &'a str,
    },
}

fn provider_condition(status: &ProviderCatalogStatus) -> Option<ProviderCondition<'_>> {
    match status {
        ProviderCatalogStatus::Stale { message } | ProviderCatalogStatus::Failed { message } => {
            Some(ProviderCondition::Failing(message))
        }
        ProviderCatalogStatus::Unavailable {
            reason,
            message,
            fix,
        } => Some(ProviderCondition::Unavailable {
            reason: *reason,
            message,
            fix,
        }),
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

/// The row standing for a Provider's condition, which choosing re-checks.
fn retry_row<'a>(text: &str, selected: bool, width: u16, theme: &Theme) -> Line<'a> {
    Line::styled(
        truncate_to_width(
            &format!("{}{text}", if selected { "› " } else { "  " }),
            usize::from(width),
        ),
        if selected {
            theme.selection.focused
        } else {
            theme.feedback.error
        },
    )
}

fn model_row_text(model: &ModelDescriptor, selected: bool, current: bool, width: usize) -> String {
    let native = (model.display_name != model.id.as_str()).then_some(model.id.as_str());
    let mut states = current_state(current);
    if model.is_default {
        states.push(("default", "D"));
    }
    if model.availability == ModelAvailability::Unavailable {
        states.push(("unavailable", "U"));
    }
    choice_row_text(&model.display_name, native, &states, selected, width)
}

/// The state a row is in for standing for the choice in force, as a row
/// spells it in full and where it is pressed for room.
fn current_state(current: bool) -> Vec<(&'static str, &'static str)> {
    if current {
        vec![("current", "C")]
    } else {
        Vec::new()
    }
}

/// A row standing for one choice: its name, the identifier it goes by where
/// that differs, and the states it is in, each spelled in full and compactly.
/// A row pressed for room spells its states compactly before it cuts either
/// name short.
fn choice_row_text(
    name: &str,
    native: Option<&str>,
    states: &[(&'static str, &'static str)],
    selected: bool,
    width: usize,
) -> String {
    let marker = if selected { "› " } else { "  " };
    let marker_width = marker.width();
    let available = width.saturating_sub(marker_width);
    let field_count = 1 + usize::from(native.is_some()) + usize::from(!states.is_empty());
    let wide_separator = " · ";
    let compact_separator = " ";
    let full_state = (!states.is_empty()).then(|| {
        let full = states.iter().map(|(full, _)| *full).collect::<Vec<_>>();
        format!("[{}]", full.join(", "))
    });
    let compact_state = (!states.is_empty()).then(|| {
        let compact = states
            .iter()
            .map(|(_, compact)| *compact)
            .collect::<Vec<_>>();
        format!("[{}]", compact.join(","))
    });
    let minimum_text_width = 4 * (1 + usize::from(native.is_some()));
    let full_fixed = full_state.as_ref().map_or(0, |state| state.width())
        + wide_separator.width() * field_count.saturating_sub(1);
    let (separator, state) = if available >= full_fixed.saturating_add(minimum_text_width) {
        (wide_separator, full_state)
    } else {
        (compact_separator, compact_state)
    };
    let fixed = state.as_ref().map_or(0, |state| state.width())
        + separator.width() * field_count.saturating_sub(1);
    let flexible = available.saturating_sub(fixed);
    let (display_width, native_width) = native.map_or((flexible, 0), |native| {
        let native_width = native.width().min((flexible / 2).max(1));
        (flexible.saturating_sub(native_width), native_width)
    });
    let mut fields = vec![truncate_to_width(name, display_width)];
    if let Some(native) = native {
        fields.push(truncate_to_width(native, native_width));
    }
    if let Some(state) = state {
        fields.push(state);
    }
    truncate_to_width(&format!("{marker}{}", fields.join(separator)), width)
}
