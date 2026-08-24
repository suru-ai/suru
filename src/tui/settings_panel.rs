//! The settings panel: the tabs it splits its rows across, one row per Setting
//! or Provider, and the typed mutation an edit of one emits.
//!
//! The panel holds no copy of what a Setting is worth. Rows read their values
//! off the latest effective-settings snapshot, so an edit's round trip through
//! the server is what moves a row — the panel never shows a value the Config
//! Document does not yet carry.
//!
//! Two tabs split the listing. General is the schema filtered to the Settings
//! that configure no Provider, so one added there appears with no work.
//! Providers is hand-built from the built-in Provider list rather than from the
//! schema: one row per Provider, always and in the built-in order, each row
//! standing for that Provider's Enablement. Which tab a Setting lands on is the
//! Setting's own declaration in the schema, so the panel maps groups to tabs
//! and invents no grouping of its own.
//!
//! Everything else a Provider is configured by is revealed under it, as
//! ordinary rows the reader expands the Provider to see. The expansion is view
//! state and nothing more — the panel forgets it on close — so a Provider whose
//! Settings the reader is not looking at costs the listing one line.
//!
//! The Providers tab is a surface presenting the Providers themselves, so
//! arriving on it re-reads their Availability through the same Model catalog
//! listing the picker asks for. What that read finds is the panel's own to
//! keep, alongside the rest of its ephemeral state: a Provider row reads it,
//! and each arrival asks again rather than trusting the last answer. Enablement
//! decides who is asked — a Provider the reader turned off is never consulted,
//! so its row reports the choice and never a condition Suru did not look for.
//!
//! The reader may also point at the panel. Drawing is the only thing that knows
//! how tall the box came out and which rows the window put on screen, so the
//! frame leaves its geometry here and a click resolves against that, the way a
//! click in the transcript resolves against the viewport it drew. What a
//! pointer can reach stops there: a tab label shows that tab and a row takes
//! the focus, because every edit runs through the focused Setting and pointing
//! at a row is not asking for one.

use std::{cell::RefCell, ops::Range};

use crate::{
    protocol::{
        EffectiveSettings, ModelCatalog, ProviderCatalogStatus, ProviderId, ProviderUnavailability,
        SettingMutation,
    },
    provider::built_in_providers,
    settings::{SCHEMA, SettingDescriptor, SettingGroup, provider_enablement, provider_settings},
};

use super::ModelListRequest;

/// What a row reports about a Provider the read in force asked about and the
/// answer passed over. Nothing came back for it, which is Suru's problem to
/// report rather than a condition of the Provider — and reporting it is what
/// ends the read, because a row may not wait for an answer already given.
const UNANSWERED_PROVIDER: &str = "the Model catalog answered for no such Provider";

/// Stands in for a value the effective settings hold but the schema does not
/// name, so a row says what it knows rather than claiming a wrong value.
const UNNAMED_VALUE: &str = "unknown";

/// One tab of the panel, which is one group of Settings under the name the
/// reader picks it out by.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SettingsTab {
    #[default]
    General,
    Providers,
}

impl SettingsTab {
    /// Every tab, in the order the tab bar draws them and Left and Right walk
    /// them. The default is where an opening panel lands.
    pub(super) const ALL: &'static [Self] = &[Self::General, Self::Providers];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Providers => "Providers",
        }
    }

    /// The group of Settings this tab is the surface for.
    fn group(self) -> SettingGroup {
        match self {
            Self::General => SettingGroup::General,
            Self::Providers => SettingGroup::Providers,
        }
    }

    /// Where this tab sits in the bar, which is also where the panel keeps the
    /// row the reader left focused on it.
    fn position(self) -> usize {
        Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("every tab is one of ALL")
    }
}

/// Where the panel is and what it last heard, which is all the state an edit
/// needs: the values themselves live in the snapshot.
#[derive(Clone, Debug, Default)]
pub(super) struct SettingsPanel {
    open: bool,
    /// The tab the panel is showing.
    tab: SettingsTab,
    /// The focused row of each tab, kept apart so hopping to the Providers tab
    /// and back does not cost the reader their place in General. Ephemeral by
    /// design: an opening panel always starts at the first tab's top row.
    selected: [usize; SettingsTab::ALL.len()],
    /// The Providers whose further Settings are on show. Ephemeral like the
    /// selection: an opening panel expands nothing, so the reader always meets
    /// the same short list of Providers.
    expanded: Vec<&'static ProviderId>,
    /// What the Availability read in force found, one entry per Provider it
    /// asked about — which is every enabled Provider and no other. A Provider
    /// with no entry here is one nothing has been read about, and its row says
    /// as much by saying nothing.
    availability: Vec<ProviderReading>,
    /// The catalog listing those readings are waiting on. Answers to any other
    /// listing are somebody else's: a reader who left the tab and came back has
    /// asked again, and the older answer must not settle the newer question.
    awaited_listing: Option<ModelListRequest>,
    /// Why the last edit never reached the Config Document. Cleared by the
    /// next edit, so a stale complaint never outlives the attempt that earned
    /// it.
    error: Option<String>,
    /// Where the frame in force drew the tab bar and the rows, which is what a
    /// pointer resolves against. Rendering leaves it here, so it is held behind
    /// a cell rather than taken by an edit.
    layout: RefCell<PanelLayout>,
}

/// One tab as the tab bar draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SettingsTabLabel {
    pub(super) tab: SettingsTab,
    pub(super) title: &'static str,
    pub(super) active: bool,
}

/// Where a frame drew the panel's two pointable surfaces. Everything here is
/// terminal geometry the drawing decided, which is why drawing is what records
/// it; a panel nothing has drawn yet answers no click, because a layout that
/// claimed a geometry it had not drawn would move the focus somewhere the
/// reader never pointed.
#[derive(Clone, Debug, Default)]
pub(super) struct PanelLayout {
    /// The columns inside the box's border, so a click on the border or on the
    /// screen the overlay covers lands on nothing.
    columns: Range<u16>,
    /// The tab bar, where the box had the room to draw one.
    tabs: Option<TabBar>,
    /// The rows, where any were drawn at all.
    rows: Option<RowWindow>,
}

/// The tab bar as one frame drew it: the row it went on, and the columns each
/// label fills.
#[derive(Clone, Debug)]
pub(super) struct TabBar {
    pub(super) row: u16,
    pub(super) labels: Vec<TabSpan>,
}

/// One tab label as the bar drew it: the tab it names and the columns it fills.
#[derive(Clone, Debug)]
pub(super) struct TabSpan {
    pub(super) tab: SettingsTab,
    pub(super) columns: Range<u16>,
}

/// The rows as one frame drew them: where they began, which row of the tab the
/// first of them was, and how many followed. A tab is longer than the box has
/// room for, so a pointer means nothing without knowing which of its rows the
/// window put on screen.
#[derive(Clone, Copy, Debug)]
pub(super) struct RowWindow {
    pub(super) top: u16,
    pub(super) first: usize,
    pub(super) count: u16,
}

/// What the pointer landed on, which is a tab to show or a row to focus and
/// never anything a reader has to undo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelHit {
    Tab(SettingsTab),
    Row(usize),
}

impl PanelLayout {
    /// The geometry one frame drew, out of the parts the drawing decided: what
    /// a valid layout is stays here rather than with whoever assembled it.
    pub(super) fn new(columns: Range<u16>, tabs: Option<TabBar>, rows: Option<RowWindow>) -> Self {
        Self {
            columns,
            tabs,
            rows,
        }
    }

    fn hit(&self, column: u16, row: u16) -> Option<PanelHit> {
        if !self.columns.contains(&column) {
            return None;
        }
        if let Some(bar) = &self.tabs
            && bar.row == row
            && let Some(span) = bar
                .labels
                .iter()
                .find(|span| span.columns.contains(&column))
        {
            return Some(PanelHit::Tab(span.tab));
        }
        let window = self.rows?;
        let offset = row.checked_sub(window.top)?;
        (offset < window.count)
            .then(|| window.first.saturating_add(usize::from(offset)))
            .map(PanelHit::Row)
    }
}

/// One row as the panel presents it: what it is called, what it is worth, and
/// whether that value is the reader's own choice or the built-in default.
#[derive(Clone, Copy, Debug)]
pub(super) struct SettingRow {
    pub(super) label: &'static str,
    pub(super) value: RowValue,
    pub(super) pinned: bool,
    pub(super) selected: bool,
    pub(super) expansion: RowExpansion,
    pub(super) availability: RowAvailability,
}

/// What one Provider's Availability read has come to, held beside the message
/// the reader gets in full when the row is theirs to act on.
#[derive(Clone, Debug)]
struct ProviderReading {
    provider: ProviderId,
    availability: RowAvailability,
    /// What the Provider or the read itself said, whole. `None` where there is
    /// nothing to say beyond the row.
    message: Option<String>,
}

/// What arriving on a tab asks of Suru. Availability is a fact about the
/// environment rather than a choice, so the surface presenting the Providers
/// re-reads it on arrival and every other arrival asks nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum AvailabilityRead {
    /// The panel is now waiting on a catalog listing, which its caller owes it.
    Begun,
    /// Nothing was asked, so nothing is owed.
    None,
}

/// What a Provider row says about Availability. Every state but the quiet one
/// asks something of the reader, which is why the quiet one is what a Provider
/// still serving its catalog gets: Suru reports a condition, not a heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowAvailability {
    /// Nothing to report: a catalog that answered, a Provider nothing has been
    /// read about, and every row that is not a Provider's.
    Quiet,
    /// Being read right now, which is live work and so wears a Spinner.
    Reading,
    /// Unusable until the reader fixes this outside Suru.
    Unavailable(ProviderUnavailability),
    /// The read itself failed, which is Suru's problem to report rather than a
    /// condition of the Provider.
    Failed,
}

/// Where a row sits in the one nesting the panel has: a Provider that reveals
/// further Settings, and the Settings so revealed. A row says which it is
/// rather than how far to indent it, so the drawing stays the client's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowExpansion {
    /// Neither expandable nor inside an expansion: every General row.
    Absent,
    /// A Provider with nothing further to configure. It offers no affordance,
    /// but stands in the same column as the Providers that do, so a tab of
    /// Providers reads as one list.
    Unexpandable,
    /// A Provider whose further Settings are hidden.
    Collapsed,
    /// A Provider whose further Settings follow it.
    Expanded,
    /// One of those Settings, drawn under the Provider it configures.
    Revealed,
}

impl RowExpansion {
    /// Whether Enter on this row has anything to do, which is both what the
    /// key acts on and what decides whether the panel teaches it at all.
    pub(super) fn expands(self) -> bool {
        matches!(self, Self::Collapsed | Self::Expanded)
    }
}

/// What a row says it is worth, which reads differently for a Setting the
/// reader cycles through values and for a Provider whose row is its Enablement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowValue {
    /// The Setting's value, spelled as a Config Document spells it.
    Choice(&'static str),
    /// A Provider the reader has left on. The quiet state is the good one, so
    /// the row says nothing beyond the Provider's name.
    ProviderEnabled,
    /// A Provider the reader turned off — the one thing a row may claim about
    /// a Provider Suru has not consulted.
    ProviderDisabled,
}

/// One row of a tab before a snapshot values it: what it is called, the Setting
/// an edit of it acts on, and whether it stands for a Provider rather than for
/// a value to read.
struct PanelEntry {
    label: &'static str,
    descriptor: &'static SettingDescriptor,
    provider: Option<&'static ProviderId>,
    expansion: RowExpansion,
}

impl SettingsPanel {
    /// Opens the panel fresh every time: the first tab, its top row, and no
    /// complaint carried over from the last time the reader was here.
    pub(super) fn open(&mut self) {
        *self = Self {
            open: true,
            ..Self::default()
        };
    }

    pub(super) fn close(&mut self) {
        *self = Self::default();
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Takes the reason an edit never landed, so the reader learns the Config
    /// Document did not change rather than watching a row silently refuse.
    pub(super) fn report_failure(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub(super) fn select_previous_tab(&mut self, settings: &EffectiveSettings) -> AvailabilityRead {
        self.move_tab(-1, settings)
    }

    pub(super) fn select_next_tab(&mut self, settings: &EffectiveSettings) -> AvailabilityRead {
        self.move_tab(1, settings)
    }

    /// Takes the geometry the frame just drew, which is the only account of the
    /// panel a pointer can be resolved against.
    pub(super) fn record_layout(&self, layout: PanelLayout) {
        self.layout.replace(layout);
    }

    /// Gives up the geometry a frame recorded. A frame that draws no panel —
    /// because none is open, or because the terminal came out too small for one
    /// — leaves nothing to point at, so a click cannot land on where the panel
    /// used to be.
    pub(super) fn forget_layout(&self) {
        self.record_layout(PanelLayout::default());
    }

    /// Moves the panel to whatever the reader pointed at: a tab label shows
    /// that tab, a row takes the focus, and a click on anything else — the
    /// headline, the footer, the border, the screen the box is drawn over —
    /// changes nothing. A click never edits a Setting and never expands a
    /// Provider: the focused row is the one surface an edit runs through, so
    /// pointing at a row asks for the focus and never for the value.
    ///
    /// Like every other way into the panel, this does nothing at all while the
    /// panel is closed, so a click resolved somewhere the panel is not can
    /// neither move a focus the reader cannot see nor send Suru off to consult
    /// the Providers.
    ///
    /// The pointer mints no semantic command of its own, because it introduces
    /// no behavior: showing a tab and focusing a row are what the tab and row
    /// keys already do, and a plugin reaches both through those. It only says
    /// which one, by position — and a position is this frame's to know rather
    /// than something a command could name, which is why the hit test ends
    /// here rather than in a subject a command could carry.
    pub(super) fn focus_at(
        &mut self,
        column: u16,
        row: u16,
        settings: &EffectiveSettings,
    ) -> AvailabilityRead {
        if !self.open {
            return AvailabilityRead::None;
        }
        let hit = self.layout.borrow().hit(column, row);
        match hit {
            Some(PanelHit::Tab(tab)) => self.show_tab(tab, settings),
            Some(PanelHit::Row(row)) => {
                self.focus_row(row);
                AvailabilityRead::None
            }
            None => AvailabilityRead::None,
        }
    }

    /// Notes the listing the read in force is waiting on, which is the one
    /// answer that may settle it.
    pub(super) fn await_listing(&mut self, request: ModelListRequest) {
        self.awaited_listing = Some(request);
    }

    /// Takes what the awaited listing found, for the Providers this panel asked
    /// about and no others: an answer naming a Provider nothing was read about
    /// — one the reader has turned off — changes nothing here, because a row
    /// may not claim what Suru never looked for. A Provider the answer passes
    /// over has still been answered, badly, and says so rather than waiting on.
    pub(super) fn adopt_catalog(&mut self, request: &ModelListRequest, catalog: &ModelCatalog) {
        if self.awaited_listing.as_ref() != Some(request) {
            return;
        }
        for reading in &mut self.availability {
            let Some(found) = catalog
                .providers
                .iter()
                .find(|listed| listed.provider == reading.provider)
            else {
                reading.availability = RowAvailability::Failed;
                reading.message = Some(UNANSWERED_PROVIDER.to_owned());
                continue;
            };
            let (availability, message) = match &found.status {
                // A refresh the server armed is this read still running, so
                // the row keeps its Spinner until the settled answer lands.
                ProviderCatalogStatus::Refreshing => (RowAvailability::Reading, None),
                ProviderCatalogStatus::Unavailable { reason, message } => {
                    (RowAvailability::Unavailable(*reason), Some(message.clone()))
                }
                ProviderCatalogStatus::Failed { message } => {
                    (RowAvailability::Failed, Some(message.clone()))
                }
                // A catalog still serving what it last read asks nothing of
                // the reader, whether or not the newest refresh went through.
                ProviderCatalogStatus::Fresh
                | ProviderCatalogStatus::Stale { .. }
                | ProviderCatalogStatus::Disabled => (RowAvailability::Quiet, None),
            };
            reading.availability = availability;
            reading.message = message;
        }
    }

    /// Takes the reason the awaited listing never came back. It was the one
    /// thing asked of every Provider at once, so its failure is every
    /// asked-about Provider's failure.
    pub(super) fn report_read_failure(&mut self, request: &ModelListRequest, error: &str) {
        if self.awaited_listing.as_ref() != Some(request) {
            return;
        }
        for reading in &mut self.availability {
            reading.availability = RowAvailability::Failed;
            reading.message = Some(error.to_owned());
        }
    }

    /// Whether a read is still out on the tab showing it, which is what keeps
    /// the Spinner animating. A reader who has walked away from the Providers
    /// is watching nothing spin, whatever is still in flight for them.
    pub(super) fn is_reading(&self, settings: &EffectiveSettings) -> bool {
        self.open
            && self.tab == SettingsTab::Providers
            && built_in_providers().iter().any(|provider| {
                self.reading(&provider.id, settings)
                    .is_some_and(|reading| reading.availability == RowAvailability::Reading)
            })
    }

    /// What the focused Provider's Availability says in full, which the
    /// headline carries because a row has room only for the condition's name.
    pub(super) fn selected_message(&self, settings: &EffectiveSettings) -> Option<&str> {
        if !self.open {
            return None;
        }
        let entries = self.entries();
        let provider = entries.get(self.selected_row(entries.len()))?.provider?;
        self.reading(provider, settings)?.message.as_deref()
    }

    /// The tab bar: every tab, always, with the one being shown marked as such.
    pub(super) fn tabs(&self) -> Vec<SettingsTabLabel> {
        SettingsTab::ALL
            .iter()
            .map(|tab| SettingsTabLabel {
                tab: *tab,
                title: tab.title(),
                active: *tab == self.tab,
            })
            .collect()
    }

    /// The Setting an edit acts on: the focused row of an open panel, and
    /// nothing at all while the panel is closed. Every edit runs through here,
    /// so a command invoked from somewhere the panel is not — a plugin, a
    /// click — cannot reach a Setting the reader never focused.
    pub(super) fn selected_descriptor(&self) -> Option<&'static SettingDescriptor> {
        self.open.then(|| {
            let entries = self.entries();
            entries
                .get(self.selected_row(entries.len()))
                .map(|entry| entry.descriptor)
        })?
    }

    /// The active tab's rows, valued from the snapshot.
    pub(super) fn rows(&self, settings: &EffectiveSettings, pinned: &[String]) -> Vec<SettingRow> {
        let entries = self.entries();
        let selected = self.selected_row(entries.len());
        entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let (value, availability) = match entry.provider {
                    Some(provider) if settings.provider_enabled(provider) => (
                        RowValue::ProviderEnabled,
                        self.reading(provider, settings)
                            .map_or(RowAvailability::Quiet, |reading| reading.availability),
                    ),
                    // A Provider the reader turned off is one Suru leaves
                    // entirely alone, so its row reports that choice and never
                    // a condition nothing looked for.
                    Some(_) => (RowValue::ProviderDisabled, RowAvailability::Quiet),
                    // A Setting holding a value the schema does not name is a
                    // schema that fell behind its own types, not a reason to
                    // refuse the reader the rest of the panel.
                    None => (
                        RowValue::Choice(
                            entry
                                .descriptor
                                .effective(settings)
                                .map_or(UNNAMED_VALUE, |choice| choice.value),
                        ),
                        RowAvailability::Quiet,
                    ),
                };
                SettingRow {
                    label: entry.label,
                    value,
                    pinned: pinned.iter().any(|key| key == entry.descriptor.key),
                    selected: index == selected,
                    expansion: entry.expansion,
                    availability,
                }
            })
            .collect()
    }

    /// Reveals the focused Provider's further Settings beneath it, or hides
    /// them again. Enter carries no other meaning in the panel, so a row with
    /// nothing to reveal — a Setting, or a Provider configured by nothing
    /// else — is left exactly as it was rather than given a second behavior.
    /// Like an edit, this reaches a Provider only through the focused row, so a
    /// command arriving while the panel is closed expands nothing.
    pub(super) fn toggle_expansion(&mut self) {
        if !self.open {
            return;
        }
        let entries = self.entries();
        let expandable = entries
            .get(self.selected_row(entries.len()))
            .filter(|entry| entry.expansion.expands())
            .and_then(|entry| entry.provider);
        let Some(provider) = expandable else {
            return;
        };
        // Cleared only once the key has something to do, so a row Enter passes
        // over does not quietly take away the complaint the reader is reading.
        self.error = None;
        match self.expanded.iter().position(|open| *open == provider) {
            Some(index) => {
                self.expanded.remove(index);
            }
            None => self.expanded.push(provider),
        }
    }

    /// Moves the focused Setting one choice on from the value in force,
    /// wrapping past the last, and pins where it lands. On a Provider's row
    /// that Setting is its Enablement, so the same key that changes a value
    /// turns a Provider on or off. The step is measured against the snapshot
    /// rather than anything staged, because a Setting is not a transaction:
    /// each press is a whole edit that the server answers with the snapshot the
    /// next press steps from.
    pub(super) fn cycle(&mut self, settings: &EffectiveSettings) -> Option<SettingMutation> {
        self.error = None;
        let descriptor = self.selected_descriptor()?;
        Some(descriptor.next_choice(settings)?.pin)
    }

    /// Takes the focused Setting's pin out of the Config Document. The reset
    /// goes out whether or not the panel believes the Setting is pinned: the
    /// file is the server's to know, and a reset that second-guessed a stale
    /// pin list would silently refuse the one thing the reader asked for. An
    /// unset of something never pinned leaves the document untouched anyway.
    pub(super) fn reset(&mut self) -> Option<SettingMutation> {
        self.error = None;
        Some(self.selected_descriptor()?.reset)
    }

    /// The rows of the tab being shown. General is the schema filtered to its
    /// group; Providers is built from the built-in Provider list so that every
    /// Provider holds its place whatever the reader has done to it.
    fn entries(&self) -> Vec<PanelEntry> {
        let tab = self.tab;
        match tab {
            SettingsTab::General => SCHEMA
                .iter()
                .filter(|descriptor| descriptor.group == tab.group())
                .map(|descriptor| PanelEntry {
                    label: descriptor.label,
                    descriptor,
                    provider: None,
                    expansion: RowExpansion::Absent,
                })
                .collect(),
            SettingsTab::Providers => {
                let mut entries = Vec::new();
                for provider in built_in_providers() {
                    // A Provider whose Enablement the schema does not define
                    // has no row to offer, because the row is that Setting's
                    // surface. The guard beside the Provider list is what keeps
                    // that from shipping.
                    let Some(enablement) = provider_enablement(&provider.id) else {
                        continue;
                    };
                    let further = provider_settings(&provider.id);
                    let expanded = self.expanded.contains(&&provider.id);
                    entries.push(PanelEntry {
                        label: provider.display_name.as_str(),
                        descriptor: enablement,
                        provider: Some(&provider.id),
                        expansion: match (further.is_empty(), expanded) {
                            (true, _) => RowExpansion::Unexpandable,
                            (false, false) => RowExpansion::Collapsed,
                            (false, true) => RowExpansion::Expanded,
                        },
                    });
                    if expanded {
                        entries.extend(further.into_iter().map(|descriptor| PanelEntry {
                            label: descriptor.label,
                            descriptor,
                            provider: None,
                            expansion: RowExpansion::Revealed,
                        }));
                    }
                }
                entries
            }
        }
    }

    /// Where the focus sits on the tab being shown, held inside a listing of
    /// `rows`. A tab's length moves under the focus — a Provider collapsing
    /// takes rows away beneath it — so the focus is clamped where it is read
    /// rather than only where it is moved, and a shorter listing lands the
    /// reader on its last row instead of on nothing at all.
    fn selected_row(&self, rows: usize) -> usize {
        self.selected[self.tab.position()].min(rows.saturating_sub(1))
    }

    fn move_selection(&mut self, distance: isize) {
        self.error = None;
        let rows = self.entries().len();
        if rows == 0 {
            return;
        }
        let tab = self.tab.position();
        self.selected[tab] =
            (self.selected[tab] as isize + distance).rem_euclid(rows as isize) as usize;
    }

    /// Walks the tab bar, wrapping at either end so neither is a dead end. The
    /// tab arrived at is where the reader left it, not its top row. A closed
    /// panel walks nowhere: a command invoked from somewhere the panel is not
    /// must not send Suru off to consult the Providers.
    fn move_tab(&mut self, distance: isize, settings: &EffectiveSettings) -> AvailabilityRead {
        if !self.open {
            return AvailabilityRead::None;
        }
        let position = (self.tab.position() as isize + distance)
            .rem_euclid(SettingsTab::ALL.len() as isize) as usize;
        self.show_tab(SettingsTab::ALL[position], settings)
    }

    /// Shows one tab, however the reader asked for it. Asking for the tab that
    /// presents the Providers themselves re-reads their Availability, which is
    /// what turning to that surface means — including from the reader already
    /// on it, because pointing at the Providers is asking Suru to consult them,
    /// and that gesture is how a reader who has just signed in outside Suru
    /// watches the row come good. Walking the bar never lands on the tab in
    /// view, so only a pointer can ask this.
    fn show_tab(&mut self, tab: SettingsTab, settings: &EffectiveSettings) -> AvailabilityRead {
        self.error = None;
        self.tab = tab;
        if self.tab != SettingsTab::Providers {
            return AvailabilityRead::None;
        }
        self.begin_availability_read(settings);
        AvailabilityRead::Begun
    }

    /// Focuses one row of the tab being shown, which is a move like Up and Down
    /// and takes the standing complaint away as they do.
    fn focus_row(&mut self, row: usize) {
        self.error = None;
        self.selected[self.tab.position()] = row;
    }

    /// Puts every Provider that is about to be asked about back to waiting.
    /// Only the ones the reader has left on: a disabled Provider is one Suru
    /// leaves entirely alone, so nothing is asked on its behalf and its row has
    /// nothing to wait for.
    fn begin_availability_read(&mut self, settings: &EffectiveSettings) {
        self.availability = built_in_providers()
            .iter()
            .filter(|provider| settings.provider_enabled(&provider.id))
            .map(|provider| ProviderReading {
                provider: provider.id.clone(),
                availability: RowAvailability::Reading,
                message: None,
            })
            .collect();
    }

    /// What the read in force found about one Provider, and nothing at all
    /// about one the reader has turned off since. Enablement decides who is
    /// asked, so it decides who may be reported on — and it decides here once,
    /// so a row and the headline above it can never disagree about whether a
    /// Provider has anything to say.
    fn reading(
        &self,
        provider: &ProviderId,
        settings: &EffectiveSettings,
    ) -> Option<&ProviderReading> {
        if !settings.provider_enabled(provider) {
            return None;
        }
        self.availability
            .iter()
            .find(|reading| &reading.provider == provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Setting reaches the reader only through the tab its group maps to, so
    /// a group no tab lists is a Setting that has silently left the panel. The
    /// second half is what makes this bite: a tab must list the group it claims
    /// and nothing else, so a Setting cannot surface under a heading that does
    /// not describe it.
    #[test]
    fn every_setting_declares_a_group_some_tab_lists() {
        for descriptor in SCHEMA {
            assert!(
                SettingsTab::ALL
                    .iter()
                    .any(|tab| tab.group() == descriptor.group),
                "{} declares a group no settings panel tab lists",
                descriptor.key
            );
        }
        for tab in SettingsTab::ALL {
            let panel = SettingsPanel {
                open: true,
                tab: *tab,
                ..SettingsPanel::default()
            };
            for entry in panel.entries() {
                assert_eq!(
                    entry.descriptor.group,
                    tab.group(),
                    "the {} tab lists {}, which declares another group",
                    tab.title(),
                    entry.descriptor.key
                );
            }
        }
    }

    /// A Provider-scoped Setting reaches the reader as the Provider's own row
    /// or from inside that Provider's expansion, and the two must not overlap:
    /// one keyed on no built-in Provider would have left the panel silently,
    /// and an Enablement repeated inside an expansion would give the reader two
    /// rows for one value.
    #[test]
    fn every_provider_setting_has_exactly_one_row_among_the_expanded_providers() {
        let panel = SettingsPanel {
            open: true,
            tab: SettingsTab::Providers,
            expanded: built_in_providers()
                .iter()
                .map(|provider| &provider.id)
                .collect(),
            ..SettingsPanel::default()
        };
        let keys = panel
            .entries()
            .iter()
            .map(|entry| entry.descriptor.key)
            .collect::<Vec<_>>();
        for descriptor in SCHEMA
            .iter()
            .filter(|descriptor| descriptor.group == SettingGroup::Providers)
        {
            assert_eq!(
                keys.iter().filter(|key| **key == descriptor.key).count(),
                1,
                "{} does not have exactly one row on the Providers tab: {keys:?}",
                descriptor.key
            );
        }
    }

    /// The General tab is defined by exclusion — everything that configures no
    /// Provider — so a Provider Setting grouped as General would surface among
    /// the Transcript ones, and a General one grouped as a Provider's would
    /// vanish from the panel entirely.
    #[test]
    fn a_setting_is_grouped_by_whether_it_configures_a_provider() {
        for descriptor in SCHEMA {
            let expected = if descriptor.key.starts_with("provider.") {
                SettingGroup::Providers
            } else {
                SettingGroup::General
            };
            assert_eq!(
                descriptor.group, expected,
                "{} is grouped away from the tab its key says it belongs to",
                descriptor.key
            );
        }
    }
}
