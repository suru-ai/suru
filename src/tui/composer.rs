//! Client-local multiline composer memory keyed by landing route or Session.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
};

use ratatui::layout::{Position, Rect};

use super::text_layout::{CursorTarget, TextLayout};

use crate::protocol::{
    InitialPrompt, PromptId, SessionReference, SkillDescriptor, SkillInvocation, SkillMarkerSpan,
    skill_marker_matches,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum ComposerKey {
    Landing,
    Session(SessionReference),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerMemory {
    composers: HashMap<ComposerKey, ComposerState>,
    frame: RefCell<Option<ComposerFrame>>,
}

/// The editable rectangle and scroll drawn by the latest frame.
#[derive(Clone, Debug)]
struct ComposerFrame {
    key: ComposerKey,
    area: Rect,
    scroll: u16,
}

#[derive(Clone, Debug, Default)]
struct ComposerState {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    // Pointer placement may choose the end of the row before a wrap;
    // ordinary editing and navigation return to the default wrap side.
    prefer_previous_row: bool,
    history: Vec<String>,
    history_position: Option<usize>,
    history_scratch: Option<String>,
    retry: Option<RetryPrompt>,
    skill_bindings: Vec<DraftSkillBinding>,
    skill_issues: Vec<DraftSkillIssue>,
}

#[derive(Clone, Debug)]
struct DraftSkillBinding {
    invocation: SkillInvocation,
    inferred: bool,
}

#[derive(Clone, Debug)]
struct DraftSkillIssue {
    marker: Range<usize>,
    message: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerSkillMarkers {
    pub(super) recognized: Vec<Range<usize>>,
    pub(super) invalid: Vec<Range<usize>>,
}

#[derive(Clone, Debug)]
struct RetryPrompt {
    id: PromptId,
    text: String,
    skill_invocations: Vec<SkillInvocation>,
}

impl ComposerMemory {
    pub(super) fn forget_frame(&self) {
        self.frame.replace(None);
    }

    pub(super) fn record_frame(&self, key: ComposerKey, area: Rect, scroll: u16) {
        self.frame
            .replace(Some(ComposerFrame { key, area, scroll }));
    }

    pub(super) fn selection_frame(&self) -> Option<super::selection::SelectionFrame> {
        let frame = self.frame.borrow();
        let frame = frame.as_ref()?;
        let text = self.text(frame.key.clone());
        let layout = TextLayout::new(text, frame.area.width);
        Some(super::selection::SelectionFrame {
            surface: super::selection::SelectionSurface::Composer,
            area: frame.area,
            scroll: usize::from(frame.scroll),
            rows: layout
                .rows()
                .map(|row| row.start..row.start + row.text.len())
                .collect(),
            text: text.to_owned(),
        })
    }

    /// Selection offsets belong to this draft; its cursor is the focus.
    pub(super) fn selection_range(&self, key: ComposerKey) -> Option<Range<usize>> {
        let composer = self.composers.get(&key)?;
        let anchor = composer.selection_anchor?;
        let range = anchor.min(composer.cursor)..anchor.max(composer.cursor);
        (!range.is_empty() && composer.text.get(range.clone()).is_some()).then_some(range)
    }

    pub(super) fn select(&mut self, key: ComposerKey, anchor: usize, focus: usize) {
        let composer = self.composer_mut(key);
        if composer.text.is_char_boundary(anchor) && composer.text.is_char_boundary(focus) {
            composer.selection_anchor = (anchor != focus).then_some(anchor);
            composer.cursor = focus;
            composer.prefer_previous_row = false;
        }
    }

    pub(super) fn clear_selections(&mut self) {
        for composer in self.composers.values_mut() {
            composer.selection_anchor = None;
        }
    }

    pub(super) fn highlight_selection(&self, buffer: &mut ratatui::buffer::Buffer) {
        let Some(frame) = self.selection_frame() else {
            return;
        };
        let recorded = self.frame.borrow();
        let Some(recorded) = recorded.as_ref() else {
            return;
        };
        if let Some(range) = self.selection_range(recorded.key.clone()) {
            frame.highlight_range(range, buffer);
        }
    }

    pub(super) fn copy_selection(&self, key: ComposerKey) -> Option<String> {
        let range = self.selection_range(key)?;
        self.selection_frame()?.copy_range(range)
    }

    pub(super) fn hit(&self, key: ComposerKey, position: Position) -> Option<CursorTarget> {
        let frame = self.frame.borrow();
        let frame = frame.as_ref()?;
        if frame.key != key || !frame.area.contains(position) {
            return None;
        }
        Some(
            TextLayout::new(self.text(key), frame.area.width).cursor_target(
                frame.scroll.saturating_add(position.y - frame.area.y),
                position.x - frame.area.x,
            ),
        )
    }

    pub(super) fn place_cursor(&mut self, key: ComposerKey, target: CursorTarget) {
        let composer = self.composer_mut(key);
        if composer.text.is_char_boundary(target.offset) {
            composer.cursor = target.offset;
            composer.prefer_previous_row = target.prefer_previous_row;
        }
    }

    pub(super) fn prefers_previous_row(&self, key: ComposerKey) -> bool {
        self.composers
            .get(&key)
            .is_some_and(|composer| composer.prefer_previous_row)
    }

    pub(super) fn text(&self, key: ComposerKey) -> &str {
        self.composers
            .get(&key)
            .map_or("", |composer| composer.text.as_str())
    }

    pub(super) fn cursor(&self, key: ComposerKey) -> usize {
        self.composers
            .get(&key)
            .map_or(0, |composer| composer.cursor)
    }

    pub(super) fn skill_issue(&self, key: ComposerKey) -> Option<&str> {
        self.composers
            .get(&key)
            .and_then(|composer| composer.skill_issues.first())
            .map(|issue| issue.message.as_str())
    }

    pub(super) fn skill_markers(&self, key: ComposerKey) -> ComposerSkillMarkers {
        let Some(composer) = self.composers.get(&key) else {
            return ComposerSkillMarkers::default();
        };
        let invalid = composer
            .skill_issues
            .iter()
            .map(|issue| issue.marker.clone())
            .collect::<Vec<_>>();
        let recognized = composer
            .skill_bindings
            .iter()
            .map(|binding| {
                binding.invocation.marker.start as usize..binding.invocation.marker.end as usize
            })
            .filter(|range| !invalid.contains(range))
            .collect();
        ComposerSkillMarkers {
            recognized,
            invalid,
        }
    }

    pub(super) fn valid_skill_ids(&self, key: ComposerKey) -> HashSet<crate::protocol::SkillId> {
        let Some(composer) = self.composers.get(&key) else {
            return HashSet::new();
        };
        composer
            .skill_bindings
            .iter()
            .filter(|binding| {
                let marker = binding.invocation.marker.start as usize
                    ..binding.invocation.marker.end as usize;
                !composer
                    .skill_issues
                    .iter()
                    .any(|issue| issue.marker == marker)
            })
            .map(|binding| binding.invocation.skill_id.clone())
            .collect()
    }

    pub(super) fn is_empty(&self, key: ComposerKey) -> bool {
        self.text(key).is_empty()
    }

    pub(super) fn insert(&mut self, key: ComposerKey, text: &str) {
        self.composer_mut(key).insert(text);
    }

    pub(super) fn replace(&mut self, key: ComposerKey, range: Range<usize>, text: &str) -> bool {
        self.composer_mut(key).replace(range, text)
    }

    pub(super) fn insert_skill(
        &mut self,
        key: ComposerKey,
        range: Range<usize>,
        skill: &SkillDescriptor,
    ) -> bool {
        self.composer_mut(key).insert_skill(range, skill)
    }

    pub(super) fn resolve_skills(
        &mut self,
        key: ComposerKey,
        catalog: Option<&crate::protocol::SkillCatalog>,
    ) {
        self.composers
            .entry(key)
            .or_default()
            .resolve_skills(catalog);
    }

    pub(super) fn delete_backward(&mut self, key: ComposerKey) {
        self.composer_mut(key).delete_backward();
    }

    pub(super) fn delete_forward(&mut self, key: ComposerKey) {
        self.composer_mut(key).delete_forward();
    }

    pub(super) fn move_left(&mut self, key: ComposerKey) {
        self.composer_mut(key).move_left();
    }

    pub(super) fn move_right(&mut self, key: ComposerKey) {
        self.composer_mut(key).move_right();
    }

    pub(super) fn move_line_start(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        composer.cursor = composer.line_start();
    }

    pub(super) fn move_line_end(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        composer.cursor = composer.line_end();
    }

    pub(super) fn history_previous(&mut self, key: ComposerKey) {
        self.composer_mut(key).history_previous();
    }

    pub(super) fn history_next(&mut self, key: ComposerKey) {
        self.composer_mut(key).history_next();
    }

    /// Whether Down would do nothing in this composer: the caret already rests
    /// on the last line and no history walk is in progress. That one free
    /// meaning is the only one another surface may take, so caret movement and
    /// history navigation always keep theirs.
    pub(super) fn down_is_inert(&self, key: ComposerKey) -> bool {
        self.composers
            .get(&key)
            .is_none_or(ComposerState::down_is_inert)
    }

    pub(super) fn clear(&mut self, key: ComposerKey) {
        self.composer_mut(key).clear();
    }

    pub(super) fn discard_session(&mut self, session: SessionReference) {
        self.composers.remove(&ComposerKey::Session(session));
    }

    pub(super) fn begin_submission(&mut self, key: ComposerKey) -> InitialPrompt {
        self.composer_mut(key).begin_submission()
    }

    pub(super) fn admission_failed(&mut self, key: ComposerKey, prompt: &InitialPrompt) {
        self.composer_mut(key).admission_failed(prompt);
    }

    pub(super) fn admission_reconciled(
        &mut self,
        source: ComposerKey,
        destination: ComposerKey,
        prompt: &InitialPrompt,
    ) {
        self.with_migrated_composer(source, destination, |composer| {
            composer.admission_reconciled(prompt);
        });
    }

    pub(super) fn late_admission_reconciled(
        &mut self,
        source: ComposerKey,
        destination: ComposerKey,
        prompt: &InitialPrompt,
    ) -> bool {
        self.with_migrated_composer(source, destination, |composer| {
            composer.late_admission_reconciled(prompt)
        })
    }

    pub(super) fn recover_session_to_landing(&mut self, session: SessionReference) {
        let Some(mut recovered) = self
            .composers
            .remove(&ComposerKey::Session(session.clone()))
        else {
            return;
        };
        if recovered.text.is_empty() {
            self.composers
                .insert(ComposerKey::Session(session), recovered);
            return;
        }
        if let Some(landing) = self.composers.remove(&ComposerKey::Landing) {
            for entry in landing.history {
                recovered.push_history(entry);
            }
            recovered.push_history(landing.text);
        }
        self.composers.insert(ComposerKey::Landing, recovered);
    }

    /// Prepares an edit or navigation, clearing the pointer's wrap-side
    /// preference. Catalog-only refreshes bypass this because they leave the
    /// insertion point untouched.
    fn composer_mut(&mut self, key: ComposerKey) -> &mut ComposerState {
        let composer = self.composers.entry(key).or_default();
        composer.prefer_previous_row = false;
        composer
    }

    fn with_migrated_composer<T>(
        &mut self,
        source: ComposerKey,
        destination: ComposerKey,
        operation: impl FnOnce(&mut ComposerState) -> T,
    ) -> T {
        if source == destination {
            return operation(self.composer_mut(destination));
        }
        let mut composer = self.composers.remove(&source).unwrap_or_default();
        let result = operation(&mut composer);
        self.composers.insert(destination, composer);
        result
    }
}

impl ComposerState {
    fn resolve_skills(&mut self, catalog: Option<&crate::protocol::SkillCatalog>) {
        self.skill_issues.clear();
        self.skill_bindings.retain(|binding| {
            let invocation = &binding.invocation;
            let range = invocation.marker.start as usize..invocation.marker.end as usize;
            self.text
                .get(range)
                .is_some_and(|marker| skill_marker_matches(marker, &invocation.name))
        });
        let fresh_catalog = catalog.filter(|catalog| {
            matches!(
                catalog.status,
                crate::protocol::SkillCatalogStatus::Fresh { .. }
            )
        });
        let Some(catalog) = fresh_catalog else {
            for binding in &mut self.skill_bindings {
                binding.inferred = false;
                let invocation = &binding.invocation;
                self.skill_issues.push(DraftSkillIssue {
                    marker: invocation.marker.start as usize..invocation.marker.end as usize,
                    message: format!(
                        "Skill `${}` is stale; edit or choose the Skill again before submitting",
                        invocation.name
                    ),
                });
            }
            self.skill_issues.sort_by_key(|issue| issue.marker.start);
            return;
        };

        for binding in &mut self.skill_bindings {
            if binding.inferred && !skill_binding_is_current(&binding.invocation, catalog) {
                binding.inferred = false;
            }
        }
        self.skill_bindings.retain(|binding| !binding.inferred);
        for binding in &self.skill_bindings {
            let invocation = &binding.invocation;
            if !skill_binding_is_current(invocation, catalog) {
                self.skill_issues.push(DraftSkillIssue {
                    marker: invocation.marker.start as usize..invocation.marker.end as usize,
                    message: format!(
                        "Skill `${}` is stale; edit or choose the Skill again before submitting",
                        invocation.name
                    ),
                });
            }
        }
        for (range, matches) in exact_skill_markers(&self.text, &catalog.skills) {
            if self.skill_bindings.iter().any(|binding| {
                let marker = binding.invocation.marker.start as usize
                    ..binding.invocation.marker.end as usize;
                marker.start < range.end && range.start < marker.end
            }) {
                continue;
            }
            if matches.len() != 1 {
                let marker = self.text.get(range.clone()).unwrap_or("$Skill");
                self.skill_issues.push(DraftSkillIssue {
                    marker: range,
                    message: format!(
                        "There are multiple Skills named `{}`; choose a scoped result from autocomplete",
                        marker.trim_start_matches('$')
                    ),
                });
                continue;
            }
            let skill = matches[0];
            self.skill_bindings.push(DraftSkillBinding {
                invocation: SkillInvocation {
                    skill_id: skill.id.clone(),
                    name: skill.name.clone(),
                    scope: skill.scope.clone(),
                    marker: SkillMarkerSpan {
                        start: range.start as u32,
                        end: range.end as u32,
                    },
                },
                inferred: true,
            });
        }
        self.skill_bindings
            .sort_by_key(|binding| binding.invocation.marker.start);
        if let Some(limit) = catalog.capabilities.max_distinct_invocations {
            let mut admitted = HashSet::new();
            for binding in &self.skill_bindings {
                let invocation = &binding.invocation;
                if admitted.contains(&invocation.skill_id) {
                    continue;
                }
                if admitted.len() < limit as usize {
                    admitted.insert(invocation.skill_id.clone());
                    continue;
                }
                self.skill_issues.push(DraftSkillIssue {
                    marker: invocation.marker.start as usize..invocation.marker.end as usize,
                    message: format!(
                        "This Provider supports at most {limit} distinct Skill{} per Prompt",
                        if limit == 1 { "" } else { "s" }
                    ),
                });
            }
        }
        self.skill_issues.sort_by_key(|issue| issue.marker.start);
    }

    fn insert(&mut self, text: &str) {
        self.leave_history_navigation();
        self.rebase_invocations(self.cursor..self.cursor, text.len());
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.invalidate_retry_after_edit();
    }

    fn replace(&mut self, range: Range<usize>, replacement: &str) -> bool {
        if range.start > range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return false;
        }
        self.leave_history_navigation();
        self.rebase_invocations(range.clone(), replacement.len());
        let cursor = range.start + replacement.len();
        self.text.replace_range(range, replacement);
        self.cursor = cursor;
        if self
            .selection_anchor
            .is_some_and(|anchor| !self.text.is_char_boundary(anchor))
        {
            self.selection_anchor = None;
        }
        self.invalidate_retry_after_edit();
        true
    }

    fn insert_skill(&mut self, range: Range<usize>, skill: &SkillDescriptor) -> bool {
        let marker = format!("${}", skill.name);
        let replacement = format!("{marker} ");
        let start = range.start;
        if !self.replace(range, &replacement) {
            return false;
        }
        self.skill_bindings.push(DraftSkillBinding {
            invocation: SkillInvocation {
                skill_id: skill.id.clone(),
                name: skill.name.clone(),
                scope: skill.scope.clone(),
                marker: SkillMarkerSpan {
                    start: start as u32,
                    end: (start + marker.len()) as u32,
                },
            },
            inferred: false,
        });
        self.skill_bindings
            .sort_by_key(|binding| binding.invocation.marker.start);
        true
    }

    fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.leave_history_navigation();
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.rebase_invocations(previous..self.cursor, 0);
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.invalidate_retry_after_edit();
    }

    fn delete_forward(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        self.leave_history_navigation();
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| self.cursor + offset);
        self.rebase_invocations(self.cursor..next, 0);
        self.text.replace_range(self.cursor..next, "");
        self.invalidate_retry_after_edit();
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn move_right(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| self.cursor + offset);
    }

    fn history_previous(&mut self) {
        if self.line_start() != 0 {
            self.move_up();
            return;
        }
        if self.history.is_empty() {
            return;
        }
        let position = self.history_position.map_or_else(
            || {
                self.history_scratch = Some(self.text.clone());
                self.history.len() - 1
            },
            |position| position.saturating_sub(1),
        );
        self.history_position = Some(position);
        self.text.clone_from(&self.history[position]);
        self.skill_bindings.clear();
        self.cursor = self.text.len();
    }

    fn down_is_inert(&self) -> bool {
        self.line_end() == self.text.len() && self.history_position.is_none()
    }

    fn history_next(&mut self) {
        if self.line_end() != self.text.len() {
            self.move_down();
            return;
        }
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 < self.history.len() {
            let next = position + 1;
            self.history_position = Some(next);
            self.text.clone_from(&self.history[next]);
            self.skill_bindings.clear();
        } else {
            self.history_position = None;
            self.text = self.history_scratch.take().unwrap_or_default();
            self.skill_bindings.clear();
        }
        self.cursor = self.text.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.selection_anchor = None;
        self.cursor = 0;
        self.history_position = None;
        self.history_scratch = None;
        self.retry = None;
        self.skill_bindings.clear();
        self.skill_issues.clear();
    }

    fn begin_submission(&mut self) -> InitialPrompt {
        let text = self.text.clone();
        let id = self
            .retry
            .as_ref()
            .filter(|retry| {
                retry.text == text
                    && retry.skill_invocations.iter().eq(self
                        .skill_bindings
                        .iter()
                        .map(|binding| &binding.invocation))
            })
            .map_or_else(PromptId::new, |retry| retry.id);
        let skill_invocations = std::mem::take(&mut self.skill_bindings)
            .into_iter()
            .map(|binding| binding.invocation)
            .collect();
        self.skill_issues.clear();
        self.text.clear();
        self.selection_anchor = None;
        self.cursor = 0;
        self.history_position = None;
        self.history_scratch = None;
        InitialPrompt {
            id,
            text,
            skill_invocations,
        }
    }

    fn admission_failed(&mut self, prompt: &InitialPrompt) {
        if !self.text.is_empty() {
            self.push_history(self.text.clone());
        }
        self.text.clone_from(&prompt.text);
        self.skill_bindings = prompt
            .skill_invocations
            .iter()
            .cloned()
            .map(|invocation| DraftSkillBinding {
                invocation,
                inferred: false,
            })
            .collect();
        self.skill_issues.clear();
        self.cursor = self.text.len();
        self.history_position = None;
        self.history_scratch = None;
        self.retry = Some(RetryPrompt {
            id: prompt.id,
            text: prompt.text.clone(),
            skill_invocations: prompt.skill_invocations.clone(),
        });
    }

    fn admission_reconciled(&mut self, prompt: &InitialPrompt) {
        self.push_history(prompt.text.clone());
        if self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.id == prompt.id)
        {
            self.retry = None;
        }
    }

    fn late_admission_reconciled(&mut self, prompt: &InitialPrompt) -> bool {
        self.push_history(prompt.text.clone());
        let restored_was_current = self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.id == prompt.id && self.text == retry.text);
        if restored_was_current {
            self.text.clear();
            self.skill_bindings.clear();
            self.skill_issues.clear();
            self.cursor = 0;
            self.history_position = None;
            self.history_scratch = None;
        }
        if self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.id == prompt.id)
        {
            self.retry = None;
        }
        restored_was_current
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn move_up(&mut self) {
        let current_start = self.line_start();
        if current_start == 0 {
            return;
        }
        let column = self.text[current_start..self.cursor].chars().count();
        let previous_end = current_start - 1;
        let previous_start = self.text[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = byte_at_character_column(&self.text, previous_start, previous_end, column);
    }

    fn move_down(&mut self) {
        let current_start = self.line_start();
        let current_end = self.line_end();
        if current_end == self.text.len() {
            return;
        }
        let column = self.text[current_start..self.cursor].chars().count();
        let next_start = current_end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |offset| next_start + offset);
        self.cursor = byte_at_character_column(&self.text, next_start, next_end, column);
    }

    fn leave_history_navigation(&mut self) {
        self.history_position = None;
        self.history_scratch = None;
    }

    fn invalidate_retry_after_edit(&mut self) {
        if self.retry.as_ref().is_some_and(|retry| {
            retry.text != self.text
                || retry.skill_invocations.iter().ne(self
                    .skill_bindings
                    .iter()
                    .map(|binding| &binding.invocation))
        }) {
            self.retry = None;
        }
    }

    fn push_history(&mut self, text: String) {
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text);
        }
    }

    fn rebase_invocations(&mut self, edited: Range<usize>, replacement_len: usize) {
        let removed_len = edited.end.saturating_sub(edited.start);
        let delta = replacement_len as isize - removed_len as isize;
        self.skill_bindings.retain_mut(|binding| {
            let invocation = &mut binding.invocation;
            let start = invocation.marker.start as usize;
            let end = invocation.marker.end as usize;
            if edited.end <= start {
                invocation.marker.start = shift(start, delta) as u32;
                invocation.marker.end = shift(end, delta) as u32;
                true
            } else {
                edited.start >= end
            }
        });
    }
}

fn skill_binding_is_current(
    invocation: &SkillInvocation,
    catalog: &crate::protocol::SkillCatalog,
) -> bool {
    catalog.skills.iter().any(|skill| {
        skill.id == invocation.skill_id
            && skill.name == invocation.name
            && skill.scope == invocation.scope
    })
}

fn exact_skill_markers<'a>(
    text: &str,
    skills: &'a [SkillDescriptor],
) -> Vec<(Range<usize>, Vec<&'a SkillDescriptor>)> {
    let mut markers = Vec::new();
    for (start, character) in text.char_indices() {
        if character != '$' || !skill_marker_start_is_valid(text, start) {
            continue;
        }
        let marker_start = start + 1;
        let mut matches = skills
            .iter()
            .filter_map(|skill| {
                skill_match_end(text, marker_start, &skill.name).map(|end| (end, skill))
            })
            .collect::<Vec<_>>();
        let Some(end) = matches.iter().map(|(end, _)| *end).max() else {
            continue;
        };
        matches.retain(|(candidate_end, _)| *candidate_end == end);
        markers.push((
            start..end,
            matches.into_iter().map(|(_, skill)| skill).collect(),
        ));
    }
    markers
}

fn skill_match_end(text: &str, start: usize, canonical: &str) -> Option<usize> {
    let tail = text.get(start..)?;
    let canonical = canonical
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let mut visible = String::new();
    for (offset, character) in tail.char_indices() {
        visible.extend(character.to_lowercase());
        if visible.len() > canonical.len() {
            return None;
        }
        if visible == canonical {
            let end = start + offset + character.len_utf8();
            return is_skill_marker_end(text, end).then_some(end);
        }
    }
    None
}

pub(super) fn skill_marker_start_is_valid(text: &str, start: usize) -> bool {
    let before_is_word = text[..start]
        .chars()
        .next_back()
        .is_some_and(|character| character.is_alphanumeric() || character == '_');
    if before_is_word {
        return false;
    }
    let mut after = text[start + 1..].chars();
    match after.next() {
        Some(character) if matches!(character, '$' | '{' | '(') || character.is_ascii_digit() => {
            false
        }
        Some('.')
            if after
                .next()
                .is_some_and(|character| character.is_ascii_digit()) =>
        {
            false
        }
        _ => true,
    }
}

fn is_skill_marker_end(text: &str, end: usize) -> bool {
    let mut trailing = text.get(end..).into_iter().flat_map(str::chars);
    match trailing.next() {
        None => true,
        Some(character) if character.is_whitespace() => true,
        Some('.') => trailing.next().is_none_or(|character| {
            character.is_whitespace() || !(character.is_alphanumeric() || character == '_')
        }),
        Some(character) => matches!(
            character,
            ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        ),
    }
}

fn shift(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn byte_at_character_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| start + offset)
}
