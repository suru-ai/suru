//! Typed composer completion modes and their shared interaction state.

use std::{collections::HashSet, ops::Range};

use crate::protocol::{SkillCatalog, SkillCatalogStatus, SkillDescriptor, SkillId};

use super::commands::{
    AUTOCOMPLETE_LIMIT, SemanticCommandDescriptor, SemanticCommandId, command_matches, descriptor,
    fuzzy_score, slash_trigger,
};
use super::text_binding::skill_invocation_can_start;

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
    Skills,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompletionCandidate {
    Command(SemanticCommandId),
    Insert(String),
    Skill(SkillDescriptor),
}

/// The trigger, typed candidates, and selection belonging to one active
/// composer completion surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionMode {
    kind: CompletionKind,
    trigger: CompletionTrigger,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
    presentations: Vec<SkillDescriptor>,
    disabled: Vec<SkillDescriptor>,
    message: Option<String>,
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
            presentations: Vec::new(),
            disabled: Vec::new(),
            message: None,
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
            presentations: Vec::new(),
            disabled: Vec::new(),
            message: None,
        }
    }

    fn skills(
        trigger: CompletionTrigger,
        catalog: Option<&SkillCatalog>,
        bound_skills: &HashSet<SkillId>,
        selected: usize,
    ) -> Self {
        let (mut matches, presentations, message) = match catalog.map(|catalog| &catalog.status) {
            None | Some(SkillCatalogStatus::Loading) => {
                (Vec::new(), Vec::new(), Some("Loading Skills…".to_owned()))
            }
            Some(SkillCatalogStatus::Fresh { warning }) => (
                catalog.map_or_else(Vec::new, |catalog| skill_matches(&trigger.query, catalog)),
                Vec::new(),
                warning.clone(),
            ),
            Some(SkillCatalogStatus::Refreshing) => (
                Vec::new(),
                catalog.map_or_else(Vec::new, |catalog| skill_matches(&trigger.query, catalog)),
                Some("Refreshing Skills…".to_owned()),
            ),
            Some(SkillCatalogStatus::Stale { message }) => (
                Vec::new(),
                catalog.map_or_else(Vec::new, |catalog| skill_matches(&trigger.query, catalog)),
                Some(format!("Skills unavailable · {message}")),
            ),
            Some(SkillCatalogStatus::Unavailable { message }) => (
                Vec::new(),
                Vec::new(),
                Some(format!("Skills unavailable · {message}")),
            ),
        };
        let mut disabled = Vec::new();
        if let Some(limit) = catalog.and_then(|catalog| {
            matches!(catalog.status, SkillCatalogStatus::Fresh { .. })
                .then_some(catalog.capabilities.max_distinct_invocations)
                .flatten()
        }) && bound_skills.len() >= limit as usize
        {
            let (still_available, at_limit): (Vec<_>, Vec<_>) = matches
                .into_iter()
                .partition(|skill| bound_skills.contains(&skill.id));
            matches = still_available;
            disabled = at_limit;
        }
        let selected = selected.min(matches.len().saturating_sub(1));
        Self {
            kind: CompletionKind::Skills,
            trigger,
            candidates: matches
                .into_iter()
                .map(CompletionCandidate::Skill)
                .collect(),
            selected,
            presentations,
            disabled,
            message,
        }
    }

    fn is_commands_for(&self, trigger: &CompletionTrigger) -> bool {
        self.kind == CompletionKind::Commands && self.trigger == *trigger
    }

    fn is_skills_for(&self, trigger: &CompletionTrigger) -> bool {
        self.kind == CompletionKind::Skills && self.trigger == *trigger
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
            CompletionCandidate::Skill(skill) => Some(CompletionConfirmation::InsertSkill {
                replacement: self.trigger.replacement.clone(),
                skill: skill.clone(),
            }),
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            CompletionKind::Commands => " Commands ",
            CompletionKind::Insertion => " Completion ",
            CompletionKind::Skills => " Skills ",
        }
    }

    fn rows(&self) -> Vec<(bool, CompletionRow<'_>)> {
        let mut rows = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let row = match candidate {
                    CompletionCandidate::Command(command) => {
                        CompletionRow::Command(descriptor(*command))
                    }
                    CompletionCandidate::Insert(canonical) => CompletionRow::Insertion(canonical),
                    CompletionCandidate::Skill(skill) => CompletionRow::Skill(skill),
                };
                (index == self.selected, row)
            })
            .collect::<Vec<_>>();
        rows.extend(
            self.disabled
                .iter()
                .map(|skill| (false, CompletionRow::DisabledSkill(skill))),
        );
        rows.extend(
            self.presentations
                .iter()
                .map(|skill| (false, CompletionRow::StaleSkill(skill))),
        );
        rows.extend(
            self.message
                .as_deref()
                .map(|message| (false, CompletionRow::Message(message))),
        );
        rows
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CompletionConfirmation {
    InvokeSemantic(SemanticCommandId),
    Insert {
        replacement: Range<usize>,
        canonical: String,
    },
    InsertSkill {
        replacement: Range<usize>,
        skill: SkillDescriptor,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CompletionRow<'a> {
    Command(&'static SemanticCommandDescriptor),
    Insertion(&'a str),
    Skill(&'a SkillDescriptor),
    StaleSkill(&'a SkillDescriptor),
    DisabledSkill(&'a SkillDescriptor),
    Message(&'a str),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerCompletion {
    mode: Option<CompletionMode>,
    dismissed: Option<(CompletionKind, usize)>,
}

impl ComposerCompletion {
    pub(super) fn sync(
        &mut self,
        text: &str,
        cursor: usize,
        catalog: Option<&SkillCatalog>,
        bound_skills: &HashSet<SkillId>,
    ) {
        if let Some((query, replacement)) = skill_trigger(text, cursor) {
            if self.dismissed == Some((CompletionKind::Skills, replacement.start)) {
                self.hide();
                return;
            }
            self.dismissed = None;
            let trigger = CompletionTrigger::new(query, replacement);
            let selected = self
                .mode
                .as_ref()
                .filter(|mode| mode.is_skills_for(&trigger))
                .map_or(0, |mode| mode.selected);
            self.mode = Some(CompletionMode::skills(
                trigger,
                catalog,
                bound_skills,
                selected,
            ));
            return;
        }
        let Some((query, replacement)) = slash_trigger(text, cursor) else {
            self.dismissed = None;
            self.hide();
            return;
        };
        if self.dismissed == Some((CompletionKind::Commands, replacement.start)) {
            self.hide();
            return;
        }
        self.dismissed = None;
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
        self.dismissed = None;
        self.mode = Some(mode);
    }

    pub(super) fn dismiss_active(&mut self) {
        if let Some(mode) = &self.mode {
            self.dismissed = Some((mode.kind, mode.trigger.replacement.start));
        }
        self.hide();
    }

    pub(super) fn is_visible(&self) -> bool {
        self.mode
            .as_ref()
            .is_some_and(|mode| !mode.rows().is_empty())
    }

    pub(super) fn is_skill_completion(&self) -> bool {
        self.mode
            .as_ref()
            .is_some_and(|mode| mode.kind == CompletionKind::Skills)
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

fn skill_trigger(text: &str, cursor: usize) -> Option<(&str, Range<usize>)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let token_start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let start =
        text[token_start..cursor]
            .char_indices()
            .rev()
            .find_map(|(offset, character)| {
                let start = token_start + offset;
                (character == '$' && skill_invocation_can_start(text, start)).then_some(start)
            })?;
    let query = &text[start + 1..cursor];
    Some((query, start..cursor))
}

fn skill_matches(query: &str, catalog: &SkillCatalog) -> Vec<SkillDescriptor> {
    let mut matches = catalog
        .skills
        .iter()
        .filter_map(|skill| {
            let score = fuzzy_score(query, &skill.name)
                .into_iter()
                .chain(fuzzy_score(query, &skill.description).map(|score| score + 1_000))
                .min()?;
            Some((
                score,
                skill.name.to_ascii_lowercase(),
                skill.id.as_str(),
                skill,
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });
    matches
        .into_iter()
        .take(AUTOCOMPLETE_LIMIT)
        .map(|(_, _, _, skill)| skill.clone())
        .collect()
}
