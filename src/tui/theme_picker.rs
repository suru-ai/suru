//! Searchable Theme picker state and client-local preview ownership.

use crate::{protocol::SettingMutation, theme::built_in_themes};

use super::fuzzy::fuzzy_matches;

#[derive(Clone, Debug, Default)]
pub(super) struct ThemePicker {
    open: bool,
    original: String,
    query: String,
    themes: Vec<&'static str>,
    selected: usize,
    preview: Option<String>,
    awaiting_snapshot: bool,
    pin: Option<fn(String) -> SettingMutation>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ThemePickerRow<'a> {
    pub(super) name: &'a str,
    pub(super) selected: bool,
    pub(super) current: bool,
}

impl ThemePicker {
    pub(super) fn open(&mut self, current: &str, pin: fn(String) -> SettingMutation) {
        let current = if self.awaiting_snapshot {
            self.preview.as_deref().unwrap_or(current).to_owned()
        } else {
            current.to_owned()
        };
        let mut themes = built_in_themes()
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        themes.sort_by(|left, right| {
            left.to_lowercase()
                .cmp(&right.to_lowercase())
                .then_with(|| left.cmp(right))
        });
        themes.insert(0, "system");

        self.open = true;
        self.original = current;
        self.query.clear();
        self.themes = themes;
        self.selected = self
            .themes
            .iter()
            .position(|theme| *theme == self.original)
            .unwrap_or(0);
        self.preview = Some(self.original.clone());
        self.awaiting_snapshot = false;
        self.pin = Some(pin);
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.select_first_visible();
    }

    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        if self.query.is_empty() {
            self.selected = self
                .themes
                .iter()
                .position(|theme| *theme == self.original)
                .unwrap_or(0);
            self.preview = Some(self.original.clone());
        } else {
            self.select_first_visible();
        }
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

    pub(super) fn cancel(&mut self) -> Option<String> {
        let original = self.open.then(|| self.original.clone());
        self.open = false;
        self.query.clear();
        self.preview = None;
        self.awaiting_snapshot = false;
        self.pin = None;
        original
    }

    pub(super) fn confirm(&mut self) -> Option<SettingMutation> {
        if !self.visible_indices().contains(&self.selected) {
            return None;
        }
        let theme = self.themes.get(self.selected)?.to_string();
        let pin = self.pin?;
        self.open = false;
        self.query.clear();
        self.preview = Some(theme.clone());
        self.awaiting_snapshot = true;
        Some(pin(theme))
    }

    pub(super) fn settle_preview(&mut self) {
        if !self.awaiting_snapshot {
            return;
        }
        self.preview = None;
        self.awaiting_snapshot = false;
        self.pin = None;
    }

    pub(super) fn visible_rows(&self, capacity: usize) -> impl Iterator<Item = ThemePickerRow<'_>> {
        let visible = self.visible_indices();
        let selected = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        visible.into_iter().skip(start).take(capacity).map(|index| {
            let name = self.themes[index];
            ThemePickerRow {
                name,
                selected: index == self.selected,
                current: name == self.original,
            }
        })
    }

    pub(super) fn has_rows(&self) -> bool {
        !self.visible_indices().is_empty()
    }

    fn select_first_visible(&mut self) {
        if let Some(index) = self.visible_indices().first().copied() {
            self.selected = index;
            self.preview = Some(self.themes[index].to_owned());
        } else {
            self.preview = None;
        }
    }

    fn move_selection(&mut self, distance: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = (current as isize + distance).rem_euclid(visible.len() as isize) as usize;
        self.selected = visible[next];
        self.preview = Some(self.themes[self.selected].to_owned());
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.themes
            .iter()
            .enumerate()
            .filter_map(|(index, theme)| fuzzy_matches(&self.query, theme).then_some(index))
            .collect()
    }
}
