//! Cached projection of Session transcript content into renderable rows.
//!
//! Rendering happens on every input event, so this module memoizes the
//! expensive work at two levels. The whole view is keyed on the Session
//! revision and content width: unchanged frames reuse it outright. When the
//! Session does change, each transcript item keeps its rendered lines and
//! wrapped-row counts, so a streaming append only re-renders the item it
//! touched. Frames then extract just the viewport-sized window of lines
//! instead of handing the whole transcript to the terminal.

use std::{
    cell::{Ref, RefCell},
    collections::HashMap,
    hash::{Hash, Hasher},
};

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    protocol::{
        Activity, ActivityId, FileChange, InitialPrompt, Message, MessageId, MessageRole, PromptId,
        SessionId, SessionRevision, SessionSnapshot, TranscriptItem,
    },
    theme::Theme,
};

use super::markdown;

/// Source lines wrapping to more rows than this are split so ratatui's
/// u16-based scroll arithmetic stays in range.
const MAX_TRANSCRIPT_SOURCE_LINE_ROWS: usize = 32_000;

#[derive(Clone, Copy, Debug)]
pub(super) struct MessageStart {
    pub(super) message_id: MessageId,
    pub(super) row: usize,
}

/// Memoized transcript view owned by the render state. Interior mutability
/// keeps the cache transparent to callers that render from `&TuiState`.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptCache {
    view: RefCell<Option<TranscriptView>>,
}

impl TranscriptCache {
    /// Returns the transcript view for the given content, rebuilding only the
    /// parts whose inputs changed since the previous frame.
    pub(super) fn view(
        &self,
        generation: u64,
        snapshot: &SessionSnapshot,
        provisional: &[&InitialPrompt],
        theme: &Theme,
        width: u16,
    ) -> Ref<'_, TranscriptView> {
        let key = ViewKey {
            generation,
            session_id: snapshot.session.id,
            revision: snapshot.revision,
            width,
            provisional_fingerprint: provisional_fingerprint(provisional),
        };
        let needs_rebuild = self
            .view
            .borrow()
            .as_ref()
            .is_none_or(|view| view.key != key);
        if needs_rebuild {
            let mut slot = self.view.borrow_mut();
            let previous = slot.take();
            *slot = Some(rebuild(previous, key, snapshot, provisional, theme, width));
        }
        Ref::map(self.view.borrow(), |view| {
            view.as_ref().expect("transcript view was just rebuilt")
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewKey {
    generation: u64,
    session_id: SessionId,
    revision: SessionRevision,
    width: u16,
    provisional_fingerprint: u64,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptView {
    key: ViewKey,
    items: Vec<ItemView>,
    row_count: usize,
    message_starts: Vec<MessageStart>,
    /// Row offset of every source line across all items, for scroll math.
    line_starts: Vec<usize>,
}

impl TranscriptView {
    pub(super) fn row_count(&self) -> usize {
        self.row_count
    }

    pub(super) fn message_starts(&self) -> &[MessageStart] {
        &self.message_starts
    }

    /// Extracts the lines needed to render `viewport_rows` rows starting at
    /// `scroll_position`, along with the residual scroll offset into the first
    /// returned line. The result is bounded by the viewport, not the
    /// transcript.
    pub(super) fn window(
        &self,
        scroll_position: usize,
        viewport_rows: usize,
    ) -> (Vec<Line<'static>>, usize) {
        let first_line = self
            .line_starts
            .partition_point(|row| *row <= scroll_position)
            .saturating_sub(1);
        let window_start = self.line_starts.get(first_line).copied().unwrap_or(0);
        let local_scroll = scroll_position.saturating_sub(window_start);
        let rows_needed = local_scroll.saturating_add(viewport_rows);
        let mut lines = Vec::new();
        let mut rows = 0;
        let first_item = self
            .items
            .partition_point(|item| item.start_line <= first_line)
            .saturating_sub(1);
        'items: for item in &self.items[first_item.min(self.items.len())..] {
            let skip = first_line.saturating_sub(item.start_line);
            for (line, rows_of_line) in item.lines.iter().zip(&item.rows_per_line).skip(skip) {
                lines.push(line.clone());
                rows += rows_of_line;
                if rows >= rows_needed {
                    break 'items;
                }
            }
        }
        (lines, local_scroll)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ItemKey {
    Message(MessageId),
    Activity(ActivityId),
    Provisional(PromptId),
}

#[derive(Clone, Debug)]
struct ItemView {
    key: ItemKey,
    fingerprint: u64,
    lines: Vec<Line<'static>>,
    /// Wrapped row count per line at the view width, so layout is a prefix sum
    /// instead of a re-wrap.
    rows_per_line: Vec<usize>,
    start_line: usize,
    message_id: Option<MessageId>,
}

fn rebuild(
    previous: Option<TranscriptView>,
    key: ViewKey,
    snapshot: &SessionSnapshot,
    provisional: &[&InitialPrompt],
    theme: &Theme,
    width: u16,
) -> TranscriptView {
    let mut reusable: HashMap<ItemKey, ItemView> = previous
        .filter(|view| {
            view.key.generation == key.generation
                && view.key.session_id == key.session_id
                && view.key.width == key.width
        })
        .map(|view| {
            view.items
                .into_iter()
                .map(|item| (item.key, item))
                .collect()
        })
        .unwrap_or_default();
    let messages: HashMap<MessageId, &Message> = snapshot
        .messages
        .iter()
        .map(|message| (message.id, message))
        .collect();
    let activities: HashMap<ActivityId, &Activity> = snapshot
        .activities
        .iter()
        .map(|activity| (activity.id(), activity))
        .collect();

    let mut items = Vec::with_capacity(snapshot.transcript.len() + provisional.len());
    for item in &snapshot.transcript {
        match item {
            TranscriptItem::Message { message_id } => {
                let Some(message) = messages.get(message_id) else {
                    continue;
                };
                items.push(reuse_or_render(
                    &mut reusable,
                    ItemKey::Message(message.id),
                    message_fingerprint(message),
                    Some(message.id),
                    width,
                    |lines| render_message(lines, message, theme, width),
                ));
            }
            TranscriptItem::Activity { activity_id } => {
                let Some(activity) = activities.get(activity_id) else {
                    continue;
                };
                items.push(reuse_or_render(
                    &mut reusable,
                    ItemKey::Activity(activity.id()),
                    activity_fingerprint(activity),
                    None,
                    width,
                    |lines| render_activity(lines, activity, theme),
                ));
            }
        }
    }
    for prompt in provisional {
        items.push(reuse_or_render(
            &mut reusable,
            ItemKey::Provisional(prompt.id),
            prompt.text.len() as u64,
            None,
            width,
            |lines| push_user_message(lines, &prompt.text, theme, width),
        ));
    }

    let mut row_count = 0;
    let mut line_count = 0;
    let mut message_starts = Vec::new();
    let mut line_starts = Vec::new();
    for item in &mut items {
        item.start_line = line_count;
        if let Some(message_id) = item.message_id {
            message_starts.push(MessageStart {
                message_id,
                row: row_count,
            });
        }
        for rows_of_line in &item.rows_per_line {
            line_starts.push(row_count);
            row_count += rows_of_line;
        }
        line_count += item.lines.len();
    }
    TranscriptView {
        key,
        items,
        row_count,
        message_starts,
        line_starts,
    }
}

fn reuse_or_render(
    reusable: &mut HashMap<ItemKey, ItemView>,
    key: ItemKey,
    fingerprint: u64,
    message_id: Option<MessageId>,
    width: u16,
    render: impl FnOnce(&mut Vec<Line<'static>>),
) -> ItemView {
    if let Some(item) = reusable.remove(&key)
        && item.fingerprint == fingerprint
    {
        return item;
    }
    let mut rendered = Vec::new();
    render(&mut rendered);
    let mut lines = Vec::with_capacity(rendered.len());
    for line in rendered {
        split_oversized_line(line, width, &mut lines);
    }
    let rows_per_line = lines
        .iter()
        .map(|line| wrapped_line_count(line, width))
        .collect::<Vec<_>>();
    ItemView {
        key,
        fingerprint,
        lines,
        rows_per_line,
        start_line: 0,
        message_id,
    }
}

/// Message content is append-only, so length identifies it within a Session.
fn message_fingerprint(message: &Message) -> u64 {
    message.content.len() as u64
}

fn activity_fingerprint(activity: &Activity) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    match activity {
        Activity::Status { .. } | Activity::Error { .. } => {}
        Activity::Command {
            status,
            output,
            exit_status,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            output.len().hash(&mut hasher);
            exit_status.hash(&mut hasher);
        }
        Activity::FileChange {
            status, changes, ..
        } => {
            (*status as u8).hash(&mut hasher);
            for change in changes {
                match change {
                    FileChange::Add { path } => {
                        0u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                    }
                    FileChange::Delete { path } => {
                        1u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                    }
                    FileChange::Update { path, moved_to } => {
                        2u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                        moved_to.hash(&mut hasher);
                    }
                }
            }
        }
    }
    hasher.finish()
}

fn provisional_fingerprint(provisional: &[&InitialPrompt]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for prompt in provisional {
        prompt.id.hash(&mut hasher);
        prompt.text.len().hash(&mut hasher);
    }
    hasher.finish()
}

/// Removes terminal control sequences from Session content before it becomes
/// cell symbols. Provider output can carry raw ANSI escapes (colored build
/// logs, cursor movement); written verbatim they desynchronize the terminal
/// from ratatui's cell model, leaving stale characters on screen.
fn sanitize_content(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .chars()
        .any(|character| character != '\n' && (character.is_control() || character == '\u{7f}'))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.peek() {
                // CSI: parameters and intermediates are all below 0x40; the
                // sequence ends at its final byte in 0x40..=0x7e.
                Some('[') => {
                    characters.next();
                    for next in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or the ESC-backslash string terminator.
                Some(']') => {
                    characters.next();
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            if characters.peek() == Some(&'\\') {
                                characters.next();
                            }
                            break;
                        }
                    }
                }
                // Other escapes: optional intermediates 0x20..=0x2f, then one
                // final byte (covers charset designations like ESC ( B).
                Some(_) => {
                    while characters
                        .peek()
                        .is_some_and(|next| ('\u{20}'..='\u{2f}').contains(next))
                    {
                        characters.next();
                    }
                    characters.next();
                }
                None => {}
            },
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            character if character.is_control() || character == '\u{7f}' => {}
            character => out.push(character),
        }
    }
    std::borrow::Cow::Owned(out)
}

fn render_message(lines: &mut Vec<Line<'static>>, message: &Message, theme: &Theme, width: u16) {
    match message.role {
        MessageRole::User => push_user_message(lines, &message.content, theme, width),
        MessageRole::Agent => push_agent_message(lines, &message.content, theme),
    }
}

fn render_activity(lines: &mut Vec<Line<'static>>, activity: &Activity, theme: &Theme) {
    match activity {
        Activity::Status { text, .. } => push_prefixed_lines(lines, "  ", text, theme.text.subdued),
        Activity::Error { text, .. } => {
            push_prefixed_lines(lines, "  Error: ", text, theme.feedback.error)
        }
        Activity::Command {
            status,
            command,
            cwd,
            output,
            exit_status,
            ..
        } => push_command_activity(
            lines,
            *status,
            command,
            cwd.as_deref(),
            output,
            *exit_status,
            theme,
        ),
        Activity::FileChange {
            status, changes, ..
        } => push_file_change_activity(lines, *status, changes, theme),
    }
}

fn push_command_activity(
    lines: &mut Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    command: &str,
    cwd: Option<&std::path::Path>,
    output: &str,
    exit_status: Option<i32>,
    theme: &Theme,
) {
    use crate::protocol::ActivityStatus;

    let (marker, style) = match status {
        ActivityStatus::Active => ("$ ", theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", theme.feedback.success),
        ActivityStatus::Failed => ("× ", theme.feedback.error),
    };
    let command = match (status, exit_status) {
        (ActivityStatus::Failed, Some(exit_status)) => {
            format!("{command} (exit {exit_status})")
        }
        _ => command.to_owned(),
    };
    push_prefixed_lines(lines, &format!("  {marker}"), &command, style);
    if let Some(cwd) = cwd {
        push_prefixed_lines(
            lines,
            "    in ",
            cwd.to_string_lossy().as_ref(),
            theme.text.subdued,
        );
    }
    if !output.is_empty() {
        push_prefixed_lines(lines, "    ", output, theme.text.subdued);
    }
}

fn push_file_change_activity(
    lines: &mut Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    changes: &[FileChange],
    theme: &Theme,
) {
    use crate::protocol::ActivityStatus;

    let (marker, label, style) = match status {
        ActivityStatus::Active => ("… ", "Applying file changes", theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", "Applied file changes", theme.feedback.success),
        ActivityStatus::Failed => ("× ", "Failed to apply file changes", theme.feedback.error),
    };
    push_prefixed_lines(lines, &format!("  {marker}"), label, style);
    for change in changes {
        let summary = match change {
            FileChange::Add { path } => format!("A {}", path.to_string_lossy()),
            FileChange::Delete { path } => format!("D {}", path.to_string_lossy()),
            FileChange::Update {
                path,
                moved_to: Some(moved_to),
            } => format!(
                "R {} → {}",
                path.to_string_lossy(),
                moved_to.to_string_lossy()
            ),
            FileChange::Update {
                path,
                moved_to: None,
            } => format!("M {}", path.to_string_lossy()),
        };
        push_prefixed_lines(lines, "    ", &summary, theme.text.subdued);
    }
}

fn push_user_message(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    theme: &Theme,
    available_width: u16,
) {
    let content = sanitize_content(content);
    let surface = theme.surface.elevated.patch(theme.text.primary);
    let accent = theme.surface.elevated.patch(theme.accent.primary);
    let available_width = usize::from(available_width);
    let content_width = available_width.saturating_sub(2).max(1);
    for content_line in wrapped_content_lines(&content, content_width) {
        let padding = available_width.saturating_sub(2 + content_line.width());
        lines.push(Line::from(vec![
            Span::styled("┃ ", accent),
            Span::styled(content_line, surface),
            Span::styled(" ".repeat(padding), surface),
        ]));
    }
    lines.push(Line::default());
}

fn wrapped_content_lines(content: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for source_line in content.split('\n') {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for character in source_line.chars() {
            let character_width = character.width().unwrap_or(1);
            if line_width > 0 && line_width + character_width > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
        wrapped.push(line);
    }
    wrapped
}

fn push_agent_message(lines: &mut Vec<Line<'static>>, content: &str, theme: &Theme) {
    let content = sanitize_content(content);
    for mut line in markdown::render(&content, theme) {
        if !line.spans.is_empty() {
            line.spans.insert(0, Span::styled("  ", theme.text.primary));
        }
        lines.push(line);
    }
    if !content.is_empty() {
        lines.push(Line::default());
    }
}

fn push_prefixed_lines(lines: &mut Vec<Line<'static>>, prefix: &str, content: &str, style: Style) {
    let content = sanitize_content(content);
    for (index, line) in content.lines().enumerate() {
        lines.push(Line::styled(
            format!("{}{line}", if index == 0 { prefix } else { "  " }),
            style,
        ));
    }
}

fn wrapped_line_count(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn split_oversized_line(line: Line<'static>, width: u16, output: &mut Vec<Line<'static>>) {
    if wrapped_line_count(&line, width) <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push(line);
        return;
    }
    let character_count = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    if character_count < 2 {
        output.push(line);
        return;
    }
    let (left, right) = split_line_at_character_midpoint(line, character_count);
    split_oversized_line(left, width, output);
    split_oversized_line(right, width, output);
}

fn split_line_at_character_midpoint(
    line: Line<'static>,
    character_count: usize,
) -> (Line<'static>, Line<'static>) {
    let Line {
        style,
        alignment,
        spans,
    } = line;
    let mut remaining_left = character_count / 2;
    let mut left_spans = Vec::new();
    let mut right_spans = Vec::new();
    for span in spans {
        if remaining_left == 0 {
            right_spans.push(span);
            continue;
        }
        let span_character_count = span.content.chars().count();
        if span_character_count <= remaining_left {
            remaining_left -= span_character_count;
            left_spans.push(span);
            continue;
        }

        let content = span.content.into_owned();
        let split_byte = content
            .char_indices()
            .nth(remaining_left)
            .map_or(content.len(), |(index, _)| index);
        let (left, right) = content.split_at(split_byte);
        if !left.is_empty() {
            left_spans.push(Span::styled(left.to_owned(), span.style));
        }
        if !right.is_empty() {
            right_spans.push(Span::styled(right.to_owned(), span.style));
        }
        remaining_left = 0;
    }

    (
        Line {
            style,
            alignment,
            spans: left_spans,
        },
        Line {
            style,
            alignment,
            spans: right_spans,
        },
    )
}
