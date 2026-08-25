//! Typed composer completion modes and their shared interaction state.

use std::ops::Range;

use super::commands::{
    SemanticCommandDescriptor, SemanticCommandId, command_matches, descriptor, slash_trigger,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompletionTrigger {
    query: String,
    replacement: Range<usize>,
}

impl CompletionTrigger {
    fn new(query: impl Into<String>, replacement: Range<usize>) -> Self {
        Self {
            query: query.into(),
            replacement,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionKind {
    Commands,
    Insertion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompletionCandidate {
    Command(SemanticCommandId),
    Insert(String),
}

/// The trigger, typed candidates, and selection belonging to one active
/// composer completion surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionMode {
    kind: CompletionKind,
    trigger: CompletionTrigger,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

impl CompletionMode {
    /// Builds an inert insertion mode for a trigger already identified by a
    /// caller. Production Skill completion will supply the same data from its
    /// catalog-backed matcher.
    pub fn insertion(
        query: impl Into<String>,
        replacement: Range<usize>,
        canonical: impl Into<String>,
    ) -> Self {
        Self {
            kind: CompletionKind::Insertion,
            trigger: CompletionTrigger::new(query, replacement),
            candidates: vec![CompletionCandidate::Insert(canonical.into())],
            selected: 0,
        }
    }

    fn commands(
        trigger: CompletionTrigger,
        matches: Vec<SemanticCommandId>,
        selected: usize,
    ) -> Self {
        Self {
            kind: CompletionKind::Commands,
            trigger,
            candidates: matches
                .into_iter()
                .map(CompletionCandidate::Command)
                .collect(),
            selected,
        }
    }

    fn is_commands_for(&self, trigger: &CompletionTrigger) -> bool {
        self.kind == CompletionKind::Commands && self.trigger == *trigger
    }

    fn select_previous(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.candidates.len() - 1);
        }
    }

    fn select_next(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected + 1) % self.candidates.len();
        }
    }

    fn confirmation(&self) -> Option<CompletionConfirmation> {
        match self.candidates.get(self.selected)? {
            CompletionCandidate::Command(command) => {
                Some(CompletionConfirmation::InvokeSemantic(*command))
            }
            CompletionCandidate::Insert(canonical) => Some(CompletionConfirmation::Insert {
                replacement: self.trigger.replacement.clone(),
                canonical: canonical.clone(),
            }),
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            CompletionKind::Commands => " Commands ",
            CompletionKind::Insertion => " Completion ",
        }
    }

    fn rows(&self) -> Vec<(bool, CompletionRow<'_>)> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let row = match candidate {
                    CompletionCandidate::Command(command) => {
                        CompletionRow::Command(descriptor(*command))
                    }
                    CompletionCandidate::Insert(canonical) => CompletionRow::Insertion(canonical),
                };
                (index == self.selected, row)
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CompletionConfirmation {
    InvokeSemantic(SemanticCommandId),
    Insert {
        replacement: Range<usize>,
        canonical: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CompletionRow<'a> {
    Command(&'static SemanticCommandDescriptor),
    Insertion(&'a str),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerCompletion {
    mode: Option<CompletionMode>,
    dismissed_text: Option<String>,
}

impl ComposerCompletion {
    pub(super) fn sync(&mut self, text: &str, cursor: usize) {
        if self.dismissed_text.as_deref() != Some(text) {
            self.dismissed_text = None;
        }
        let Some((query, replacement)) = slash_trigger(text, cursor) else {
            self.hide();
            return;
        };
        if self.dismissed_text.as_deref() == Some(text) {
            self.hide();
            return;
        }
        let trigger = CompletionTrigger::new(query, replacement);
        let selected = self
            .mode
            .as_ref()
            .filter(|mode| mode.is_commands_for(&trigger))
            .map_or(0, |mode| mode.selected);
        let matches = command_matches(&trigger.query);
        let selected = selected.min(matches.len().saturating_sub(1));
        self.mode = Some(CompletionMode::commands(trigger, matches, selected));
    }

    pub(super) fn activate(&mut self, mode: CompletionMode) {
        self.dismissed_text = None;
        self.mode = Some(mode);
    }

    pub(super) fn dismiss_for_text(&mut self, text: &str) {
        self.dismissed_text = Some(text.to_owned());
        self.hide();
    }

    pub(super) fn is_visible(&self) -> bool {
        self.mode
            .as_ref()
            .is_some_and(|mode| !mode.candidates.is_empty())
    }

    pub(super) fn selected_confirmation(&self) -> Option<CompletionConfirmation> {
        self.mode.as_ref()?.confirmation()
    }

    pub(super) fn select_previous(&mut self) {
        if let Some(mode) = self.mode.as_mut() {
            mode.select_previous();
        }
    }

    pub(super) fn select_next(&mut self) {
        if let Some(mode) = self.mode.as_mut() {
            mode.select_next();
        }
    }

    pub(super) fn title(&self) -> &'static str {
        self.mode
            .as_ref()
            .map_or(" Completion ", CompletionMode::title)
    }

    pub(super) fn rows(&self) -> Vec<(bool, CompletionRow<'_>)> {
        self.mode
            .as_ref()
            .map_or_else(Vec::new, CompletionMode::rows)
    }

    pub(super) fn visible_rows(&self, capacity: usize) -> Vec<(bool, CompletionRow<'_>)> {
        let start = self
            .mode
            .as_ref()
            .map_or(0, |mode| mode.selected.saturating_add(1))
            .saturating_sub(capacity);
        self.rows().into_iter().skip(start).take(capacity).collect()
    }

    fn hide(&mut self) {
        self.mode = None;
    }
}
