//! Client-local multiline composer memory keyed by landing route or Session.

use std::collections::HashMap;

use crate::protocol::{InitialPrompt, PromptId, SessionId};

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
}

#[derive(Clone, Debug)]
struct RetryPrompt {
    id: PromptId,
    text: String,
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
        let composer = self.composer_mut(key);
        composer.leave_history_navigation();
        composer.text.insert_str(composer.cursor, text);
        composer.cursor += text.len();
        composer.invalidate_retry_after_edit();
    }

    pub(super) fn delete_backward(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.cursor == 0 {
            return;
        }
        composer.leave_history_navigation();
        let previous = composer.text[..composer.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        composer.text.replace_range(previous..composer.cursor, "");
        composer.cursor = previous;
        composer.invalidate_retry_after_edit();
    }

    pub(super) fn delete_forward(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.cursor == composer.text.len() {
            return;
        }
        composer.leave_history_navigation();
        let next = composer.text[composer.cursor..]
            .char_indices()
            .nth(1)
            .map_or(composer.text.len(), |(offset, _)| composer.cursor + offset);
        composer.text.replace_range(composer.cursor..next, "");
        composer.invalidate_retry_after_edit();
    }

    pub(super) fn move_left(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.cursor == 0 {
            return;
        }
        composer.cursor = composer.text[..composer.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(super) fn move_right(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.cursor == composer.text.len() {
            return;
        }
        composer.cursor = composer.text[composer.cursor..]
            .char_indices()
            .nth(1)
            .map_or(composer.text.len(), |(offset, _)| composer.cursor + offset);
    }

    pub(super) fn history_previous(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.line_start() != 0 {
            composer.move_up();
            return;
        }
        if composer.history.is_empty() {
            return;
        }
        let position = composer.history_position.map_or_else(
            || {
                composer.history_scratch = Some(composer.text.clone());
                composer.history.len() - 1
            },
            |position| position.saturating_sub(1),
        );
        composer.history_position = Some(position);
        composer.text.clone_from(&composer.history[position]);
        composer.cursor = composer.text.len();
    }

    pub(super) fn history_next(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        if composer.line_end() != composer.text.len() {
            composer.move_down();
            return;
        }
        let Some(position) = composer.history_position else {
            return;
        };
        if position + 1 < composer.history.len() {
            let next = position + 1;
            composer.history_position = Some(next);
            composer.text.clone_from(&composer.history[next]);
        } else {
            composer.history_position = None;
            composer.text = composer.history_scratch.take().unwrap_or_default();
        }
        composer.cursor = composer.text.len();
    }

    pub(super) fn clear(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        composer.text.clear();
        composer.cursor = 0;
        composer.history_position = None;
        composer.history_scratch = None;
        composer.retry = None;
    }

    pub(super) fn begin_submission(&mut self, key: ComposerKey) -> InitialPrompt {
        let composer = self.composer_mut(key);
        let text = composer.text.clone();
        let id = composer
            .retry
            .as_ref()
            .filter(|retry| retry.text == text)
            .map_or_else(PromptId::new, |retry| retry.id);
        composer.text.clear();
        composer.cursor = 0;
        composer.history_position = None;
        composer.history_scratch = None;
        InitialPrompt { id, text }
    }

    pub(super) fn admission_failed(&mut self, key: ComposerKey, prompt: &InitialPrompt) {
        let composer = self.composer_mut(key);
        if !composer.text.is_empty() {
            composer.push_history(composer.text.clone());
        }
        composer.text.clone_from(&prompt.text);
        composer.cursor = composer.text.len();
        composer.history_position = None;
        composer.history_scratch = None;
        composer.retry = Some(RetryPrompt {
            id: prompt.id,
            text: prompt.text.clone(),
        });
    }

    pub(super) fn admission_reconciled(
        &mut self,
        source: ComposerKey,
        destination: ComposerKey,
        prompt: &InitialPrompt,
    ) {
        if source != destination {
            let mut composer = self.composers.remove(&source).unwrap_or_default();
            composer.push_history(prompt.text.clone());
            if composer
                .retry
                .as_ref()
                .is_some_and(|retry| retry.id == prompt.id)
            {
                composer.retry = None;
            }
            self.composers.insert(destination, composer);
            return;
        }
        let composer = self.composer_mut(destination);
        composer.push_history(prompt.text.clone());
        if composer
            .retry
            .as_ref()
            .is_some_and(|retry| retry.id == prompt.id)
        {
            composer.retry = None;
        }
    }

    fn composer_mut(&mut self, key: ComposerKey) -> &mut ComposerState {
        self.composers.entry(key).or_default()
    }
}

impl ComposerState {
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
        if self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.text != self.text)
        {
            self.retry = None;
        }
    }

    fn push_history(&mut self, text: String) {
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text);
        }
    }
}

fn byte_at_character_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| start + offset)
}
