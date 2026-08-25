//! Client-local multiline composer memory keyed by landing route or Session.

use std::collections::HashMap;
use std::ops::Range;

use crate::protocol::{
    InitialPrompt, PromptId, SessionId, SkillDescriptor, SkillInvocation, SkillMarkerSpan,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ComposerKey {
    Landing,
    Session(SessionId),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerMemory {
    composers: HashMap<ComposerKey, ComposerState>,
}

#[derive(Clone, Debug, Default)]
struct ComposerState {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_position: Option<usize>,
    history_scratch: Option<String>,
    retry: Option<RetryPrompt>,
    skill_invocations: Vec<SkillInvocation>,
}

#[derive(Clone, Debug)]
struct RetryPrompt {
    id: PromptId,
    text: String,
    skill_invocations: Vec<SkillInvocation>,
}

impl ComposerMemory {
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

    pub(super) fn history_previous(&mut self, key: ComposerKey) {
        self.composer_mut(key).history_previous();
    }

    pub(super) fn history_next(&mut self, key: ComposerKey) {
        self.composer_mut(key).history_next();
    }

    pub(super) fn clear(&mut self, key: ComposerKey) {
        self.composer_mut(key).clear();
    }

    pub(super) fn discard_session(&mut self, session_id: SessionId) {
        self.composers.remove(&ComposerKey::Session(session_id));
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

    pub(super) fn recover_session_to_landing(&mut self, session_id: SessionId) {
        let Some(mut recovered) = self.composers.remove(&ComposerKey::Session(session_id)) else {
            return;
        };
        if recovered.text.is_empty() {
            self.composers
                .insert(ComposerKey::Session(session_id), recovered);
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

    fn composer_mut(&mut self, key: ComposerKey) -> &mut ComposerState {
        self.composers.entry(key).or_default()
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
        self.skill_invocations.push(SkillInvocation {
            skill_id: skill.id.clone(),
            name: skill.name.clone(),
            scope: skill.scope.clone(),
            marker: SkillMarkerSpan {
                start: start as u32,
                end: (start + marker.len()) as u32,
            },
        });
        self.skill_invocations
            .sort_by_key(|invocation| invocation.marker.start);
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
        self.skill_invocations.clear();
        self.cursor = self.text.len();
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
            self.skill_invocations.clear();
        } else {
            self.history_position = None;
            self.text = self.history_scratch.take().unwrap_or_default();
            self.skill_invocations.clear();
        }
        self.cursor = self.text.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_position = None;
        self.history_scratch = None;
        self.retry = None;
        self.skill_invocations.clear();
    }

    fn begin_submission(&mut self) -> InitialPrompt {
        let text = self.text.clone();
        let id = self
            .retry
            .as_ref()
            .filter(|retry| retry.text == text && retry.skill_invocations == self.skill_invocations)
            .map_or_else(PromptId::new, |retry| retry.id);
        let skill_invocations = std::mem::take(&mut self.skill_invocations);
        self.text.clear();
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
        self.skill_invocations.clone_from(&prompt.skill_invocations);
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
            self.skill_invocations.clear();
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
            retry.text != self.text || retry.skill_invocations != self.skill_invocations
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
        self.skill_invocations.retain_mut(|invocation| {
            let start = invocation.marker.start as usize;
            let end = invocation.marker.end as usize;
            if edited.end <= start {
                invocation.marker.start = shift(start, delta) as u32;
                invocation.marker.end = shift(end, delta) as u32;
                true
            } else if edited.start >= end {
                true
            } else {
                false
            }
        });
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
