//! The settings panel: one row per defined Setting, and the typed mutation an
//! edit of one emits.
//!
//! The panel holds no copy of what a Setting is worth. Rows are generated from
//! the compile-time schema and read their values off the latest
//! effective-settings snapshot, so a Setting added to the schema appears here
//! with no work, and an edit's round trip through the server is what moves a
//! row — the panel never shows a value the Config Document does not yet carry.

use crate::{
    protocol::{EffectiveSettings, SettingMutation},
    settings::{SCHEMA, SettingDescriptor},
};

/// Stands in for a value the effective settings hold but the schema does not
/// name, so a row says what it knows rather than claiming a wrong value.
const UNNAMED_VALUE: &str = "unknown";

/// Where the panel is and what it last heard, which is all the state an edit
/// needs: the values themselves live in the snapshot.
#[derive(Clone, Debug, Default)]
pub(super) struct SettingsPanel {
    open: bool,
    selected: usize,
    /// Why the last edit never reached the Config Document. Cleared by the
    /// next edit, so a stale complaint never outlives the attempt that earned
    /// it.
    error: Option<String>,
}

/// One Setting as the panel presents it: what it is called, what it is worth,
/// and whether that value is the reader's own choice or the built-in default.
#[derive(Clone, Copy, Debug)]
pub(super) struct SettingRow {
    pub(super) label: &'static str,
    pub(super) value: &'static str,
    pub(super) pinned: bool,
    pub(super) selected: bool,
}

impl SettingsPanel {
    pub(super) fn open(&mut self) {
        self.open = true;
        self.selected = 0;
        self.error = None;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.selected = 0;
        self.error = None;
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

    /// The Setting an edit acts on: the focused row of an open panel, and
    /// nothing at all while the panel is closed. Every edit runs through here,
    /// so a command invoked from somewhere the panel is not — a plugin, a
    /// click — cannot reach a Setting the reader never focused.
    pub(super) fn selected_descriptor(&self) -> Option<&'static SettingDescriptor> {
        self.open.then(|| SCHEMA.get(self.selected))?
    }

    /// Every defined Setting, in schema order, valued from the snapshot.
    pub(super) fn rows(&self, settings: &EffectiveSettings, pinned: &[String]) -> Vec<SettingRow> {
        SCHEMA
            .iter()
            .enumerate()
            .map(|(index, descriptor)| SettingRow {
                label: descriptor.label,
                // A Setting holding a value the schema does not name is a
                // schema that fell behind its own types, not a reason to
                // refuse the reader the rest of the panel.
                value: descriptor
                    .effective(settings)
                    .map_or(UNNAMED_VALUE, |choice| choice.value),
                pinned: pinned.iter().any(|key| key == descriptor.key),
                selected: index == self.selected,
            })
            .collect()
    }

    /// Moves the focused Setting one choice on from the value in force,
    /// wrapping past the last, and pins where it lands. The step is measured
    /// against the snapshot rather than anything staged, because a Setting is
    /// not a transaction: each press is a whole edit that the server answers
    /// with the snapshot the next press steps from.
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

    fn move_selection(&mut self, distance: isize) {
        self.error = None;
        if SCHEMA.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + distance).rem_euclid(SCHEMA.len() as isize) as usize;
    }
}
