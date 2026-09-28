//! Client-local multiline composer memory keyed by landing route or Session.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
};

use ratatui::layout::{Position, Rect};

use super::{
    text_binding::{SkillIssue, TextBinding, TextBindings, UnitEdge, image_label},
    text_layout::{CursorTarget, RowDirection, TextLayout},
};

use crate::protocol::{
    AttachmentBinding, AttachmentDescriptor, AttachmentId, InitialPrompt, PromptId,
    SessionReference, SkillDescriptor,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum ComposerKey {
    Landing,
    Session(SessionReference),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SelectionMotion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComposerMemory {
    composers: HashMap<ComposerKey, ComposerState>,
    /// What each Attachment this client uploaded was stored as, for the line
    /// that describes it beneath any draft whose text binds it. An id names
    /// the same bytes wherever it is bound, so one record serves every draft.
    attachments: HashMap<AttachmentId, AttachmentDescriptor>,
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
    // Vertical movement preserves the painted terminal column even when an
    // intervening Row is too short to reach it.
    preferred_display_column: Option<u16>,
    history: Vec<String>,
    history_position: Option<usize>,
    history_scratch: Option<String>,
    /// The rejected Prompt this draft was restored from, whose id it is sent
    /// again under for as long as its text and bindings are unchanged.
    retry: Option<InitialPrompt>,
    bindings: TextBindings,
    skill_issues: Vec<SkillIssue>,
    /// The highest `N` any `[Image N]` label in this draft has used, so a
    /// label deleted is never reused before the draft is submitted or cleared.
    highest_image_number: u32,
}

/// What a composer draws over its text: each binding without an issue in its
/// kind's style, and every span a Skill issue stands against as an error.
#[derive(Clone, Debug, Default)]
pub(super) struct ComposerBindings {
    pub(super) bound: Vec<TextBinding>,
    pub(super) invalid: Vec<Range<usize>>,
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
        composer.selected_range()
    }

    pub(super) fn select(&mut self, key: ComposerKey, anchor: usize, focus: usize) {
        let composer = self.composer_mut(key);
        if composer.text.is_char_boundary(anchor) && composer.text.is_char_boundary(focus) {
            let anchor = composer.bindings.unit_boundary(anchor, UnitEdge::Nearer);
            let focus = composer.bindings.unit_boundary(focus, UnitEdge::Nearer);
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
        let recorded = self.frame.borrow();
        let Some(recorded) = recorded.as_ref() else {
            return;
        };
        let Some(range) = self.selection_range(recorded.key.clone()) else {
            return;
        };
        if let Some(frame) = self.selection_frame() {
            frame.highlight_range(range, buffer);
        }
    }

    /// Preserve the focus of an already whole selection, including reverse selections.
    pub(super) fn select_all(&mut self, key: ComposerKey) -> bool {
        let text_len = self.text(key.clone()).len();
        if text_len == 0 || self.selection_range(key.clone()) == Some(0..text_len) {
            return false;
        }
        self.select(key, 0, text_len);
        true
    }

    pub(super) fn cut_selection(&mut self, key: ComposerKey) -> Option<String> {
        let range = self.selection_range(key.clone())?;
        let text = self.text(key.clone())[range].to_owned();
        self.composer_mut(key).insert("");
        Some(text)
    }

    pub(super) fn copy_selection(&self, key: ComposerKey) -> Option<String> {
        let range = self.selection_range(key.clone())?;
        Some(super::selection::copy_text(self.text(key).get(range)?))
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
            let offset = composer
                .bindings
                .unit_boundary(target.offset, UnitEdge::Nearer);
            composer.cursor = offset;
            composer.prefer_previous_row = target.prefer_previous_row && offset == target.offset;
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

    pub(super) fn bindings(&self, key: ComposerKey) -> ComposerBindings {
        let Some(composer) = self.composers.get(&key) else {
            return ComposerBindings::default();
        };
        let invalid = composer
            .skill_issues
            .iter()
            .map(|issue| issue.span.clone())
            .collect::<Vec<_>>();
        let bound = composer
            .bindings
            .iter()
            .filter(|binding| !invalid.contains(&binding.span))
            .cloned()
            .collect();
        ComposerBindings { bound, invalid }
    }

    pub(super) fn valid_skill_ids(&self, key: ComposerKey) -> HashSet<crate::protocol::SkillId> {
        self.composers
            .get(&key)
            .map(|composer| composer.bindings.valid_skill_ids(&composer.skill_issues))
            .unwrap_or_default()
    }

    pub(super) fn is_empty(&self, key: ComposerKey) -> bool {
        self.text(key).is_empty()
    }

    /// Keeps a draft under `key`, empty where nothing was written there yet,
    /// so work begun for it can tell later whether it is still there.
    pub(super) fn keep_draft(&mut self, key: ComposerKey) {
        self.composers.entry(key).or_default();
    }

    /// Whether a draft is still kept under `key`: one discarded with its
    /// Session, or carried to the Session its Prompt began, is gone, and
    /// nothing arriving for it may make it again.
    pub(super) fn has_draft(&self, key: &ComposerKey) -> bool {
        self.composers.contains_key(key)
    }

    /// How many Attachments the draft's text binds.
    pub(super) fn attachment_count(&self, key: ComposerKey) -> usize {
        self.composers
            .get(&key)
            .map_or(0, |composer| composer.bindings.attachments().count())
    }

    /// Writes the next `[Image N]` label and a space where the draft's cursor
    /// stands, bound to the Attachment `descriptor` describes.
    pub(super) fn insert_attachment(&mut self, key: ComposerKey, descriptor: AttachmentDescriptor) {
        self.composer_mut(key)
            .insert_attachment(descriptor.id.clone());
        self.attachments.insert(descriptor.id.clone(), descriptor);
    }

    /// One line per Attachment the draft binds, in text order, describing
    /// what its label stands for.
    pub(super) fn attachment_lines(&self, key: ComposerKey) -> Vec<String> {
        let Some(composer) = self.composers.get(&key) else {
            return Vec::new();
        };
        composer
            .bindings
            .attachment_lines(|id| self.attachments.get(id))
    }

    /// What each Attachment `bindings` name was stored as, where this client
    /// uploaded it, once each and in id order: all a Session this client has
    /// only claimed knows to describe its Prompt's Attachments with.
    pub(super) fn uploaded(&self, bindings: &[AttachmentBinding]) -> Vec<AttachmentDescriptor> {
        let mut uploaded = Vec::with_capacity(bindings.len());
        crate::session_projection::describe_attachments(
            &mut uploaded,
            bindings
                .iter()
                .filter_map(|binding| self.attachments.get(&binding.attachment_id))
                .cloned(),
        );
        uploaded
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
        let range = self.selection_range(key.clone());
        let composer = self.composer_mut(key);
        composer.selection_anchor = None;
        if let Some(range) = range {
            composer.cursor = range.start;
        } else {
            composer.move_left();
        }
    }

    pub(super) fn move_right(&mut self, key: ComposerKey) {
        let range = self.selection_range(key.clone());
        let composer = self.composer_mut(key);
        composer.selection_anchor = None;
        if let Some(range) = range {
            composer.cursor = range.end;
        } else {
            composer.move_right();
        }
    }

    pub(super) fn move_line_start(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        composer.selection_anchor = None;
        composer.cursor = composer.line_start();
    }

    pub(super) fn move_line_end(&mut self, key: ComposerKey) {
        let composer = self.composer_mut(key);
        composer.selection_anchor = None;
        composer.cursor = composer.line_end();
    }

    /// Extending vertically uses painted Rows and never enters history or a
    /// picker. Home and End continue to use written Lines.
    pub(super) fn extend_selection(&mut self, key: ComposerKey, motion: SelectionMotion) {
        let width = self.layout_width(key.clone());
        let composer = match motion {
            SelectionMotion::Up | SelectionMotion::Down => self.composers.entry(key).or_default(),
            _ => self.composer_mut(key),
        };
        composer.selection_anchor.get_or_insert(composer.cursor);
        match motion {
            SelectionMotion::Left => composer.move_left(),
            SelectionMotion::Right => composer.move_right(),
            SelectionMotion::Up if !composer.move_vertical(width, RowDirection::Previous) => {
                composer.cursor = 0;
                composer.prefer_previous_row = false;
            }
            SelectionMotion::Up => {}
            SelectionMotion::Down if !composer.move_vertical(width, RowDirection::Next) => {
                composer.cursor = composer.text.len();
                composer.prefer_previous_row = false;
            }
            SelectionMotion::Down => {}
            SelectionMotion::LineStart => composer.cursor = composer.line_start(),
            SelectionMotion::LineEnd => composer.cursor = composer.line_end(),
        }
    }

    pub(super) fn history_previous(&mut self, key: ComposerKey) {
        let width = self.layout_width(key.clone());
        let composer = self.composers.entry(key).or_default();
        composer.selection_anchor = None;
        composer.history_previous(width);
    }

    pub(super) fn history_next(&mut self, key: ComposerKey) {
        let width = self.layout_width(key.clone());
        let composer = self.composers.entry(key).or_default();
        composer.selection_anchor = None;
        composer.history_next(width);
    }

    /// Whether Down would do nothing in this composer: the caret already rests
    /// on the last painted Row and no history walk is in progress. That one free
    /// meaning is the only one another surface may take, so caret movement and
    /// history navigation always keep theirs.
    pub(super) fn down_is_inert(&self, key: ComposerKey) -> bool {
        let width = self.layout_width(key.clone());
        self.composers
            .get(&key)
            .is_none_or(|composer| composer.down_is_inert(width))
    }

    pub(super) fn clear(&mut self, key: ComposerKey) {
        self.composer_mut(key).clear();
    }

    pub(super) fn discard_session(&mut self, session: SessionReference) {
        self.composers.remove(&ComposerKey::Session(session));
    }

    /// Hands a Prompt back to the composer it was written in, cursor at its
    /// end, ready to be sent again as the very Prompt it was.
    ///
    /// A composer the reader has since written in keeps what they wrote: work
    /// coming back to them is never worth a draft they are in the middle of, so
    /// the returned text waits in that composer's history instead.
    pub(super) fn return_prompt(&mut self, key: ComposerKey, prompt: &InitialPrompt) {
        let composer = self.composer_mut(key);
        if composer.text.is_empty() {
            composer.admission_failed(prompt);
        } else {
            composer.push_history(prompt.text.clone());
        }
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
        composer.preferred_display_column = None;
        composer
    }

    fn layout_width(&self, key: ComposerKey) -> u16 {
        self.frame
            .borrow()
            .as_ref()
            .filter(|frame| frame.key == key)
            .map_or(u16::MAX, |frame| frame.area.width.max(1))
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
        self.bindings.retain_recognized(&self.text);
        self.skill_issues = self.bindings.resolve_skills(&self.text, catalog);
    }

    /// The selected text, taking whole any unit either end reaches into.
    fn selected_range(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        let range = self
            .bindings
            .covering_units(anchor.min(self.cursor)..anchor.max(self.cursor));
        (!range.is_empty() && self.text.get(range.clone()).is_some()).then_some(range)
    }

    fn insert(&mut self, text: &str) {
        let range = self.selected_range().unwrap_or(self.cursor..self.cursor);
        self.replace(range, text);
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
        self.bindings.follow_edit(range.clone(), replacement.len());
        let cursor = range.start + replacement.len();
        self.text.replace_range(range, replacement);
        self.cursor = cursor;
        self.selection_anchor = None;
        self.invalidate_retry_after_edit();
        true
    }

    fn insert_skill(&mut self, range: Range<usize>, skill: &SkillDescriptor) -> bool {
        let written = format!("${}", skill.name);
        let replacement = format!("{written} ");
        let start = range.start;
        if !self.replace(range, &replacement) {
            return false;
        }
        self.bindings
            .bind_skill(start..start + written.len(), skill);
        true
    }

    fn insert_attachment(&mut self, attachment_id: AttachmentId) {
        let number = self
            .highest_image_number
            .max(self.bindings.highest_image_number())
            + 1;
        self.highest_image_number = number;
        let label = image_label(number);
        let range = self.selected_range().unwrap_or(self.cursor..self.cursor);
        let start = range.start;
        if self.replace(range, &format!("{label} ")) {
            let span = start..start + label.len();
            self.bindings.bind_attachment(span, attachment_id, label);
        }
    }

    fn delete_backward(&mut self) {
        if let Some(range) = self.selected_range() {
            self.replace(range, "");
            return;
        }
        if self.cursor == 0 {
            self.selection_anchor = None;
            return;
        }
        if let Some(unit) = self.bindings.unit_before(self.cursor) {
            self.replace(unit, "");
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.replace(previous..self.cursor, "");
    }

    fn delete_forward(&mut self) {
        if let Some(range) = self.selected_range() {
            self.replace(range, "");
            return;
        }
        if self.cursor == self.text.len() {
            self.selection_anchor = None;
            return;
        }
        if let Some(unit) = self.bindings.unit_after(self.cursor) {
            self.replace(unit, "");
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| self.cursor + offset);
        self.replace(self.cursor..next, "");
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(unit) = self.bindings.unit_before(self.cursor) {
            self.cursor = unit.start;
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
        if let Some(unit) = self.bindings.unit_after(self.cursor) {
            self.cursor = unit.end;
            return;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| self.cursor + offset);
    }

    fn vertical_target(&self, width: u16, direction: RowDirection) -> Option<(CursorTarget, u16)> {
        let layout = TextLayout::new(&self.text, width);
        let column = self.preferred_display_column.unwrap_or_else(|| {
            layout
                .cursor_position_with_affinity(self.cursor, self.prefer_previous_row)
                .1
        });
        layout
            .adjacent_cursor_target(self.cursor, self.prefer_previous_row, column, direction)
            .map(|target| (target, column))
    }

    fn move_vertical(&mut self, width: u16, direction: RowDirection) -> bool {
        let Some((target, column)) = self.vertical_target(width, direction) else {
            return false;
        };
        let edge = match direction {
            RowDirection::Previous => UnitEdge::Start,
            RowDirection::Next => UnitEdge::End,
        };
        self.cursor = self.bindings.unit_boundary(target.offset, edge);
        self.prefer_previous_row = target.prefer_previous_row && self.cursor == target.offset;
        self.preferred_display_column = Some(column);
        true
    }

    fn history_previous(&mut self, width: u16) {
        if self.move_vertical(width, RowDirection::Previous) {
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
        self.bindings.clear();
        self.cursor = self.text.len();
        self.prefer_previous_row = false;
        self.preferred_display_column = None;
    }

    fn down_is_inert(&self, width: u16) -> bool {
        self.vertical_target(width, RowDirection::Next).is_none() && self.history_position.is_none()
    }

    fn history_next(&mut self, width: u16) {
        if self.move_vertical(width, RowDirection::Next) {
            return;
        }
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 < self.history.len() {
            let next = position + 1;
            self.history_position = Some(next);
            self.text.clone_from(&self.history[next]);
            self.bindings.clear();
        } else {
            self.history_position = None;
            self.text = self.history_scratch.take().unwrap_or_default();
            self.bindings.clear();
        }
        self.cursor = self.text.len();
        self.prefer_previous_row = false;
        self.preferred_display_column = None;
    }

    fn clear(&mut self) {
        self.text.clear();
        self.selection_anchor = None;
        self.cursor = 0;
        self.history_position = None;
        self.history_scratch = None;
        self.retry = None;
        self.bindings.clear();
        self.skill_issues.clear();
        self.highest_image_number = 0;
    }

    fn begin_submission(&mut self) -> InitialPrompt {
        let text = std::mem::take(&mut self.text);
        let bindings = std::mem::take(&mut self.bindings);
        self.highest_image_number = 0;
        let id = self
            .retry
            .as_ref()
            .filter(|retry| retry.text == text && bindings.are_carried_by(retry))
            .map_or_else(PromptId::new, |retry| retry.id);
        self.skill_issues.clear();
        self.selection_anchor = None;
        self.cursor = 0;
        self.history_position = None;
        self.history_scratch = None;
        bindings.into_prompt(id, text)
    }

    fn admission_failed(&mut self, prompt: &InitialPrompt) {
        if !self.text.is_empty() {
            self.push_history(self.text.clone());
        }
        self.text.clone_from(&prompt.text);
        self.selection_anchor = None;
        self.bindings = TextBindings::from_prompt(prompt);
        self.highest_image_number = self.bindings.highest_image_number();
        self.skill_issues.clear();
        self.cursor = self.text.len();
        self.history_position = None;
        self.history_scratch = None;
        self.retry = Some(prompt.clone());
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
            self.selection_anchor = None;
            self.bindings.clear();
            self.highest_image_number = 0;
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

    fn leave_history_navigation(&mut self) {
        self.history_position = None;
        self.history_scratch = None;
    }

    fn invalidate_retry_after_edit(&mut self) {
        if self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.text != self.text || !self.bindings.are_carried_by(retry))
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
