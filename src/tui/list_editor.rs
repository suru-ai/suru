//! The list editor: the settings panel's surface for a Setting holding a list
//! of strings, which the Setting declares whole as a
//! [`TextListSettingChoice`], so the editor never knows which Setting it
//! edits.
//!
//! Its rows are the list's entries, with one more beneath them to add an entry
//! at, and the focused row is the one typing lands on, as each line of a text
//! is. Every edit — a row added, edited or removed — is a draft until saving
//! hands the whole list to the Setting, which makes it one mutation or names
//! the first entry it refuses. A row the reader emptied is no entry at all, so
//! clearing a row and removing it come to the same list.
//!
//! A list whose entries name directories on the Server is also edited by
//! browsing: a directory chosen in the Directory Browser stands in the focused
//! entry's place, or, chosen from the row past the last entry, is added there.

use crate::{
    protocol::{EffectiveSettings, SettingMutation},
    settings::TextListSettingChoice,
};

use super::list_window::ListWindow;

/// An open list editor and the draft it holds.
#[derive(Clone, Debug)]
pub(super) struct ListEditor {
    choice: TextListSettingChoice,
    entries: Vec<String>,
    /// The row typing lands on: an entry, or the row past the last of them,
    /// where a new entry is added.
    focus: usize,
    /// Why the last save pinned nothing. Cleared by the next edit or move, so
    /// a complaint never outlives the attempt that earned it.
    error: Option<String>,
    window: ListWindow,
}

/// The editor as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct ListEditorView<'a> {
    pub(super) label: &'static str,
    /// What the row a new entry is added at reads before anything is typed.
    pub(super) placeholder: &'static str,
    /// Whether each entry names a directory, which the Directory Browser
    /// may choose.
    pub(super) names_directories: bool,
    pub(super) entries: &'a [String],
    /// The focused row, where `entries.len()` is the row a new entry is added
    /// at.
    pub(super) focus: usize,
    pub(super) error: Option<&'a str>,
    pub(super) window: &'a ListWindow,
}

impl ListEditor {
    /// Opens on the entries in force, focused on the first row.
    pub(super) fn open(choice: TextListSettingChoice, settings: &EffectiveSettings) -> Self {
        let window = ListWindow::default();
        window.open();
        Self {
            choice,
            entries: choice.seed(settings),
            focus: 0,
            error: None,
            window,
        }
    }

    pub(super) fn view(&self) -> ListEditorView<'_> {
        ListEditorView {
            label: self.choice.label(),
            placeholder: self.choice.placeholder(),
            names_directories: self.names_directories(),
            entries: &self.entries,
            focus: self.focus,
            error: self.error.as_deref(),
            window: &self.window,
        }
    }

    /// Whether each entry names a directory, which the Directory Browser
    /// may choose.
    pub(super) fn names_directories(&self) -> bool {
        self.choice.names_directories()
    }

    /// Puts the directory the reader chose in the Directory Browser in the
    /// focused entry's place, or, where focus stands on the row past the last
    /// entry, adds it there, which opens a new row to add at beneath it as
    /// typing does. Either way focus stays on the directory chosen.
    pub(super) fn choose_directory(&mut self, directory: String) {
        self.error = None;
        match self.entries.get_mut(self.focus) {
            Some(entry) => *entry = directory,
            None => self.entries.push(directory),
        }
        self.window.reveal();
    }

    pub(super) fn select_previous(&mut self) {
        self.move_focus(self.rows() - 1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_focus(1);
    }

    /// Adds `text` to the end of the focused row. Typed on the row past the
    /// last entry, it makes that row an entry, and a new row to add at opens
    /// beneath it. A line break or any other control character is no part of
    /// an entry, so a pasted line lands as the text it holds.
    pub(super) fn insert(&mut self, text: &str) {
        let text = text
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        if text.is_empty() {
            return;
        }
        self.error = None;
        match self.entries.get_mut(self.focus) {
            Some(entry) => entry.push_str(&text),
            None => self.entries.push(text),
        }
    }

    /// Takes the last character off the focused entry.
    pub(super) fn delete_backward(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.focus) {
            entry.pop();
            self.error = None;
        }
    }

    /// Removes the focused entry, leaving focus on the row that took its
    /// place.
    pub(super) fn remove(&mut self) {
        if self.focus < self.entries.len() {
            self.entries.remove(self.focus);
            self.error = None;
        }
    }

    /// The one mutation pinning every entry the draft holds, or nothing where
    /// the Setting refuses one of them — in which case focus moves to that
    /// entry and the editor says why.
    pub(super) fn save(&mut self) -> Option<SettingMutation> {
        let kept = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.is_empty())
            .collect::<Vec<_>>();
        let list = kept.iter().map(|(_, entry)| (*entry).clone()).collect();
        match self.choice.accept(list) {
            Ok(mutation) => Some(mutation),
            Err(refused) => {
                self.focus = kept[refused.index].0;
                self.window.reveal();
                self.error = Some(refused.reason);
                None
            }
        }
    }

    /// Every row: each entry, and the one a new entry is added at.
    fn rows(&self) -> usize {
        self.entries.len() + 1
    }

    /// Moves focus `distance` rows on, wrapping at either end so neither is a
    /// dead end.
    fn move_focus(&mut self, distance: usize) {
        self.error = None;
        self.focus = (self.focus + distance) % self.rows();
        self.window.reveal();
    }
}
