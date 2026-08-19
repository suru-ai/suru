//! Typed semantic commands and slash autocomplete state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const AUTOCOMPLETE_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticCommandId {
    SessionNew,
}

impl SemanticCommandId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionNew => "session.new",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SemanticCommandDescriptor {
    pub(super) id: SemanticCommandId,
    pub(super) title: &'static str,
    pub(super) description: &'static str,
    pub(super) slash: Option<SlashCommand>,
    pub(super) keybinding: Option<SemanticKeybinding>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SlashCommand {
    pub(super) name: &'static str,
    aliases: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SemanticKeybinding {
    prefix_code: KeyCode,
    prefix_modifiers: KeyModifiers,
    code: KeyCode,
    modifiers: KeyModifiers,
    pub(super) label: &'static str,
}

const SEMANTIC_COMMANDS: &[SemanticCommandDescriptor] = &[SemanticCommandDescriptor {
    id: SemanticCommandId::SessionNew,
    title: "New Session",
    description: "Open a fresh landing composer without ending the current Session",
    slash: Some(SlashCommand {
        name: "new",
        aliases: &["clear"],
    }),
    keybinding: Some(SemanticKeybinding {
        prefix_code: KeyCode::Char('x'),
        prefix_modifiers: KeyModifiers::CONTROL,
        code: KeyCode::Char('n'),
        modifiers: KeyModifiers::NONE,
        label: "Ctrl+X N",
    }),
}];

pub(super) fn descriptor(id: SemanticCommandId) -> &'static SemanticCommandDescriptor {
    SEMANTIC_COMMANDS
        .iter()
        .find(|command| command.id == id)
        .expect("every semantic command ID has one descriptor")
}

pub(super) fn command_for_leader_key(key: KeyEvent) -> Option<SemanticCommandId> {
    SEMANTIC_COMMANDS.iter().find_map(|command| {
        command.keybinding.and_then(|binding| {
            (binding.prefix_code == KeyCode::Char('x')
                && binding.prefix_modifiers == KeyModifiers::CONTROL
                && binding.code == key.code
                && binding.modifiers == key.modifiers)
                .then_some(command.id)
        })
    })
}

#[derive(Clone, Debug, Default)]
pub(super) struct CommandAutocomplete {
    query: Option<String>,
    dismissed_text: Option<String>,
    matches: Vec<SemanticCommandId>,
    selected: usize,
}

impl CommandAutocomplete {
    pub(super) fn sync(&mut self, text: &str, cursor: usize) {
        if self.dismissed_text.as_deref() != Some(text) {
            self.dismissed_text = None;
        }
        let Some(query) = slash_query(text, cursor) else {
            self.hide();
            return;
        };
        if self.dismissed_text.as_deref() == Some(text) {
            self.hide();
            return;
        }
        if self.query.as_deref() != Some(query) {
            self.selected = 0;
        }
        self.query = Some(query.to_owned());
        self.matches = autocomplete_matches(query);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub(super) fn dismiss_for_text(&mut self, text: &str) {
        self.dismissed_text = Some(text.to_owned());
        self.hide();
    }

    pub(super) fn is_visible(&self) -> bool {
        !self.matches.is_empty()
    }

    pub(super) fn selected(&self) -> Option<SemanticCommandId> {
        self.matches.get(self.selected).copied()
    }

    pub(super) fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
        }
    }

    pub(super) fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub(super) fn rows(
        &self,
    ) -> impl ExactSizeIterator<Item = (bool, &'static SemanticCommandDescriptor)> + '_ {
        self.matches
            .iter()
            .enumerate()
            .map(|(index, id)| (index == self.selected, descriptor(*id)))
    }

    pub(super) fn visible_rows(
        &self,
        capacity: usize,
    ) -> impl Iterator<Item = (bool, &'static SemanticCommandDescriptor)> + '_ {
        let start = self.selected.saturating_add(1).saturating_sub(capacity);
        self.rows().skip(start).take(capacity)
    }

    fn hide(&mut self) {
        self.query = None;
        self.matches.clear();
        self.selected = 0;
    }
}

fn slash_query(text: &str, cursor: usize) -> Option<&str> {
    if cursor != text.len() || text.contains('\n') {
        return None;
    }
    let query = text.strip_prefix('/')?;
    (!query.chars().any(char::is_whitespace)).then_some(query)
}

fn autocomplete_matches(query: &str) -> Vec<SemanticCommandId> {
    let mut matches = SEMANTIC_COMMANDS
        .iter()
        .filter_map(|command| {
            let slash = command.slash?;
            let score = fuzzy_score(query, slash.name)
                .into_iter()
                .chain(
                    slash
                        .aliases
                        .iter()
                        .filter_map(|alias| fuzzy_score(query, alias).map(|score| score + 10)),
                )
                .chain(fuzzy_score(query, command.title).map(|score| score + 20))
                .chain(fuzzy_score(query, command.description).map(|score| score + 20))
                .min()?;
            Some((score, slash.name, command.id.as_str(), command.id))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable_by_key(|(score, slash, id, _)| (*score, *slash, *id));
    matches
        .into_iter()
        .take(AUTOCOMPLETE_LIMIT)
        .map(|(_, _, _, id)| id)
        .collect()
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    if candidate == query {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(100 + candidate.len().saturating_sub(query.len()));
    }
    if let Some(index) = candidate.find(&query) {
        return Some(200 + index);
    }

    let mut positions = candidate.char_indices();
    let mut previous = None;
    let mut gaps = 0;
    for query_character in query.chars() {
        let (position, _) =
            positions.find(|(_, candidate_character)| *candidate_character == query_character)?;
        if let Some(previous) = previous {
            gaps += position.saturating_sub(previous + 1);
        }
        previous = Some(position);
    }
    Some(300 + gaps)
}
