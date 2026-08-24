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

use crate::{
    protocol::{EffectiveSettings, ProviderId, SettingMutation},
    provider::built_in_providers,
    settings::{SCHEMA, SettingDescriptor, SettingGroup, provider_enablement, provider_settings},
};

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
    /// Why the last edit never reached the Config Document. Cleared by the
    /// next edit, so a stale complaint never outlives the attempt that earned
    /// it.
    error: Option<String>,
}

/// One tab as the tab bar draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SettingsTabLabel {
    pub(super) title: &'static str,
    pub(super) active: bool,
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

    pub(super) fn select_previous_tab(&mut self) {
        self.move_tab(-1);
    }

    pub(super) fn select_next_tab(&mut self) {
        self.move_tab(1);
    }

    /// The tab bar: every tab, always, with the one being shown marked as such.
    pub(super) fn tabs(&self) -> Vec<SettingsTabLabel> {
        SettingsTab::ALL
            .iter()
            .map(|tab| SettingsTabLabel {
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
            .map(|(index, entry)| SettingRow {
                label: entry.label,
                value: match entry.provider {
                    Some(provider) if settings.provider_enabled(provider) => {
                        RowValue::ProviderEnabled
                    }
                    Some(_) => RowValue::ProviderDisabled,
                    // A Setting holding a value the schema does not name is a
                    // schema that fell behind its own types, not a reason to
                    // refuse the reader the rest of the panel.
                    None => RowValue::Choice(
                        entry
                            .descriptor
                            .effective(settings)
                            .map_or(UNNAMED_VALUE, |choice| choice.value),
                    ),
                },
                pinned: pinned.iter().any(|key| key == entry.descriptor.key),
                selected: index == selected,
                expansion: entry.expansion,
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
    /// tab arrived at is where the reader left it, not its top row.
    fn move_tab(&mut self, distance: isize) {
        self.error = None;
        let position = (self.tab.position() as isize + distance)
            .rem_euclid(SettingsTab::ALL.len() as isize) as usize;
        self.tab = SettingsTab::ALL[position];
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
