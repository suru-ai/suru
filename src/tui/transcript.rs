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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    ansi::{
        AnsiScanner, FragmentRole, TRUNCATION_MARKER, sgr_parameter_code, sgr_parameters,
        split_truncation_marker,
    },
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
            theme: *theme,
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
    theme: Theme,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TranscriptLink {
    pub(super) target: String,
}

impl TranscriptView {
    pub(super) fn row_count(&self) -> usize {
        self.row_count
    }

    pub(super) fn message_starts(&self) -> &[MessageStart] {
        &self.message_starts
    }

    /// Parsed hyperlink targets retained for future semantic commands and
    /// pointer hit-testing.
    #[allow(dead_code)]
    pub(super) fn links(&self) -> impl Iterator<Item = &TranscriptLink> {
        self.items.iter().flat_map(|item| item.links.iter())
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
    links: Vec<TranscriptLink>,
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
                && view.key.theme == key.theme
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
                    |lines, _links| render_message(lines, message, theme, width),
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
                    |lines, links| render_activity(lines, links, activity, theme),
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
            |lines, _links| push_user_message(lines, &prompt.text, theme, width),
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
    render: impl FnOnce(&mut Vec<Line<'static>>, &mut Vec<TranscriptLink>),
) -> ItemView {
    if let Some(item) = reusable.remove(&key)
        && item.fingerprint == fingerprint
    {
        return item;
    }
    let mut rendered = Vec::new();
    let mut links = Vec::new();
    render(&mut rendered, &mut links);
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
        links,
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

enum ContentToken {
    Text(String),
    Sgr(String),
    LinkStart(String),
    LinkEnd,
    Tab,
    LineBreak,
}

/// Reduces Session content to printable text, layout controls, and supported
/// style/link transitions. Every escape/control sequence omitted here stays
/// invisible in both the strip-only and styled rendering paths.
fn content_tokens(text: &str) -> impl Iterator<Item = ContentToken> {
    AnsiScanner::default()
        .feed(text)
        .into_iter()
        .filter_map(|fragment| match fragment.into_role() {
            FragmentRole::Text(text) => Some(ContentToken::Text(text)),
            FragmentRole::Sgr(sequence) => Some(ContentToken::Sgr(sequence)),
            FragmentRole::Hyperlink(hyperlink) => Some(match hyperlink.target() {
                Some(target) => ContentToken::LinkStart(target.to_owned()),
                None => ContentToken::LinkEnd,
            }),
            FragmentRole::Tab => Some(ContentToken::Tab),
            FragmentRole::LineBreak => Some(ContentToken::LineBreak),
            FragmentRole::CarriageReturn | FragmentRole::Invisible => None,
        })
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
    for token in content_tokens(text) {
        match token {
            ContentToken::Text(text) => out.push_str(&text),
            ContentToken::Tab => out.push_str("    "),
            ContentToken::LineBreak => out.push('\n'),
            ContentToken::Sgr(_) | ContentToken::LinkStart(_) | ContentToken::LinkEnd => {}
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

fn render_activity(
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<TranscriptLink>,
    activity: &Activity,
    theme: &Theme,
) {
    let mut projection = ActivityProjection { lines, links };
    match activity {
        Activity::Status { text, .. } => {
            push_styled_prefixed_lines(&mut projection, "  ", text, theme.text.subdued, theme)
        }
        Activity::Error { text, .. } => push_styled_prefixed_lines(
            &mut projection,
            "  Error: ",
            text,
            theme.feedback.error,
            theme,
        ),
        Activity::Command {
            status,
            command,
            cwd,
            output,
            exit_status,
            ..
        } => push_command_activity(
            &mut projection,
            *status,
            command,
            cwd.as_deref(),
            output,
            *exit_status,
            theme,
        ),
        Activity::FileChange {
            status, changes, ..
        } => push_file_change_activity(projection.lines, *status, changes, theme),
    }
}

struct ActivityProjection<'a> {
    lines: &'a mut Vec<Line<'static>>,
    links: &'a mut Vec<TranscriptLink>,
}

fn push_command_activity(
    projection: &mut ActivityProjection<'_>,
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
    push_prefixed_lines(projection.lines, &format!("  {marker}"), &command, style);
    if let Some(cwd) = cwd {
        push_prefixed_lines(
            projection.lines,
            "    in ",
            cwd.to_string_lossy().as_ref(),
            theme.text.subdued,
        );
    }
    let (output, truncated) = split_truncation_marker(output);
    if !output.is_empty() {
        push_styled_prefixed_lines(projection, "    ", output, theme.text.subdued, theme);
    }
    if truncated {
        push_truncation_marker(projection.lines, "    ", theme);
    }
}

/// Renders the marker the normalizer left on capped content as its own line, in
/// a style Suru applies rather than one the stream can set, so a reader can tell
/// Suru dropped the rest rather than the Provider ending there.
fn push_truncation_marker(lines: &mut Vec<Line<'static>>, indent: &str, theme: &Theme) {
    lines.push(Line::styled(
        format!("{indent}{TRUNCATION_MARKER}"),
        theme.text.subdued.add_modifier(Modifier::ITALIC),
    ));
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
    let (content, truncated) = split_truncation_marker(content);
    let content = sanitize_content(content);
    for mut line in markdown::render(&content, theme) {
        if !line.spans.is_empty() {
            line.spans.insert(0, Span::styled("  ", theme.text.primary));
        }
        lines.push(line);
    }
    if truncated {
        push_truncation_marker(lines, "  ", theme);
    }
    if !content.is_empty() || truncated {
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

fn push_styled_prefixed_lines(
    projection: &mut ActivityProjection<'_>,
    prefix: &str,
    content: &str,
    base_style: Style,
    theme: &Theme,
) {
    let mut style = SgrStyle::new(base_style);
    let mut hyperlink_active = false;
    let mut spans = vec![Span::styled(prefix.to_owned(), base_style)];
    let mut line_has_content = false;

    for token in content_tokens(content) {
        match token {
            ContentToken::Text(text) if !text.is_empty() => {
                spans.push(Span::styled(
                    text,
                    activity_content_style(style.rendered, hyperlink_active, theme),
                ));
                line_has_content = true;
            }
            ContentToken::Sgr(sequence) => apply_sgr(&sequence, &mut style, base_style, theme),
            ContentToken::LinkStart(target) => {
                projection.links.push(TranscriptLink { target });
                hyperlink_active = true;
            }
            ContentToken::LinkEnd => hyperlink_active = false,
            ContentToken::Tab => {
                spans.push(Span::styled(
                    "    ",
                    activity_content_style(style.rendered, hyperlink_active, theme),
                ));
                line_has_content = true;
            }
            ContentToken::LineBreak => {
                projection
                    .lines
                    .push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled("  ", base_style));
                line_has_content = false;
            }
            ContentToken::Text(_) => {}
        }
    }
    if line_has_content {
        projection.lines.push(Line::from(spans));
    }
}

#[derive(Clone, Copy, Debug)]
enum AnsiForeground {
    Normal(u16),
    Bright,
}

#[derive(Clone, Copy, Debug)]
struct SgrStyle {
    rendered: Style,
    ansi_foreground: Option<AnsiForeground>,
    bold_active: bool,
}

impl SgrStyle {
    fn new(base_style: Style) -> Self {
        Self {
            rendered: base_style,
            ansi_foreground: None,
            bold_active: false,
        }
    }

    fn reset(&mut self, base_style: Style) {
        *self = Self::new(base_style);
    }

    fn set_bold(&mut self, theme: &Theme) {
        self.bold_active = true;
        self.rendered = self.rendered.add_modifier(Modifier::BOLD);
        if let Some(AnsiForeground::Normal(index)) = self.ansi_foreground {
            self.rendered.fg = theme.ansi.color(index, true);
        }
    }

    fn reset_intensity(&mut self, theme: &Theme) {
        self.bold_active = false;
        self.rendered = self
            .rendered
            .remove_modifier(Modifier::BOLD | Modifier::DIM);
        if let Some(AnsiForeground::Normal(index)) = self.ansi_foreground {
            self.rendered.fg = theme.ansi.color(index, false);
        }
    }

    fn set_normal_foreground(&mut self, index: u16, theme: &Theme) {
        self.ansi_foreground = Some(AnsiForeground::Normal(index));
        self.rendered.fg = theme.ansi.color(index, self.bold_active);
    }

    fn set_bright_foreground(&mut self, index: u16, theme: &Theme) {
        self.ansi_foreground = Some(AnsiForeground::Bright);
        self.rendered.fg = theme.ansi.color(index, true);
    }

    fn clear_ansi_foreground(&mut self) {
        self.ansi_foreground = None;
    }
}

fn activity_content_style(style: Style, hyperlink_active: bool, theme: &Theme) -> Style {
    if hyperlink_active {
        style.patch(theme.markdown.link)
    } else {
        style
    }
}

fn apply_sgr(sequence: &str, style: &mut SgrStyle, base_style: Style, theme: &Theme) {
    let Some(parameters) = sgr_parameters(sequence) else {
        return;
    };
    let parameters: Vec<&str> = parameters.collect();
    let mut index = 0;
    while index < parameters.len() {
        if parameters[index].contains(':') {
            if parameters[index].starts_with("38:") {
                style.clear_ansi_foreground();
            }
            apply_colon_color(parameters[index], &mut style.rendered);
            index += 1;
            continue;
        }
        let Some(parameter) = sgr_parameter_code(parameters[index]) else {
            index += 1;
            continue;
        };
        match parameter {
            0 => style.reset(base_style),
            1 => style.set_bold(theme),
            2 => style.rendered = style.rendered.add_modifier(Modifier::DIM),
            3 => style.rendered = style.rendered.add_modifier(Modifier::ITALIC),
            4 => style.rendered = style.rendered.add_modifier(Modifier::UNDERLINED),
            7 => style.rendered = style.rendered.add_modifier(Modifier::REVERSED),
            22 => style.reset_intensity(theme),
            23 => style.rendered = style.rendered.remove_modifier(Modifier::ITALIC),
            24 => style.rendered = style.rendered.remove_modifier(Modifier::UNDERLINED),
            27 => style.rendered = style.rendered.remove_modifier(Modifier::REVERSED),
            30..=37 => style.set_normal_foreground(parameter - 30, theme),
            38 => {
                style.clear_ansi_foreground();
                index = apply_extended_color(&parameters, index, &mut style.rendered.fg);
                continue;
            }
            39 => {
                style.clear_ansi_foreground();
                style.rendered.fg = base_style.fg;
            }
            40..=47 => style.rendered.bg = theme.ansi.color(parameter - 40, false),
            48 => {
                index = apply_extended_color(&parameters, index, &mut style.rendered.bg);
                continue;
            }
            49 => style.rendered.bg = base_style.bg,
            90..=97 => style.set_bright_foreground(parameter - 90, theme),
            100..=107 => style.rendered.bg = theme.ansi.color(parameter - 100, true),
            _ => {}
        }
        index += 1;
    }
}

fn apply_extended_color(parameters: &[&str], index: usize, target: &mut Option<Color>) -> usize {
    match parameters
        .get(index + 1)
        .and_then(|parameter| sgr_parameter_code(parameter))
    {
        Some(5) => {
            if let Some(value) = parameters
                .get(index + 2)
                .and_then(|parameter| sgr_parameter_code(parameter))
                .and_then(|value| u8::try_from(value).ok())
            {
                *target = Some(Color::Indexed(value));
            }
            (index + 3).min(parameters.len())
        }
        Some(2) => {
            if let (Some(red), Some(green), Some(blue)) = (
                parameters.get(index + 2).copied().and_then(sgr_byte),
                parameters.get(index + 3).copied().and_then(sgr_byte),
                parameters.get(index + 4).copied().and_then(sgr_byte),
            ) {
                *target = Some(Color::Rgb(red, green, blue));
            }
            (index + 5).min(parameters.len())
        }
        Some(_) => (index + 2).min(parameters.len()),
        None => index + 1,
    }
}

fn apply_colon_color(parameter: &str, style: &mut Style) {
    let parameters: Vec<Option<u16>> = parameter
        .split(':')
        .map(|parameter| {
            if parameter.is_empty() {
                None
            } else {
                parameter.parse().ok()
            }
        })
        .collect();
    let [
        Some(color_target @ (38 | 48)),
        Some(color_kind),
        values @ ..,
    ] = parameters.as_slice()
    else {
        return;
    };
    let color = match color_kind {
        5 => values
            .first()
            .copied()
            .flatten()
            .and_then(|value| u8::try_from(value).ok())
            .map(Color::Indexed),
        2 => {
            let rgb = if values.len() >= 4 {
                &values[1..]
            } else {
                values
            };
            let (red, green, blue) = (
                rgb.first().copied().flatten(),
                rgb.get(1).copied().flatten(),
                rgb.get(2).copied().flatten(),
            );
            match (red, green, blue) {
                (Some(red), Some(green), Some(blue)) => {
                    match (u8::try_from(red), u8::try_from(green), u8::try_from(blue)) {
                        (Ok(red), Ok(green), Ok(blue)) => Some(Color::Rgb(red, green, blue)),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };
    match (*color_target, color) {
        (38, Some(color)) => style.fg = Some(color),
        (48, Some(color)) => style.bg = Some(color),
        _ => {}
    }
}

fn sgr_byte(parameter: &str) -> Option<u8> {
    parameter.parse().ok()
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{
        style::{Color, Modifier},
        text::Line,
    };

    use crate::{
        ansi::TRUNCATION_MARKER,
        protocol::{
            Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
            ModelAvailability, Session, SessionRevision, SessionSnapshot, SessionStatus,
            TranscriptItem, TurnId, Workspace,
        },
        theme::Theme,
    };

    use super::{TranscriptCache, render_activity, render_message};

    #[test]
    fn activity_base_ansi_colors_follow_theme_palette() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show colors".to_owned(),
            cwd: None,
            output: "\x1b[31;44mnormal\x1b[91;104mbright".to_owned(),
            exit_status: Some(0),
        };
        let mut theme = Theme::system();
        theme.ansi.normal.red = Color::Rgb(1, 2, 3);
        theme.ansi.normal.blue = Color::Rgb(4, 5, 6);
        theme.ansi.bright.red = Color::Rgb(7, 8, 9);
        theme.ansi.bright.blue = Color::Rgb(10, 11, 12);
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(&mut lines, &mut links, &activity, &theme);

        let spans = &lines.last().expect("render command output").spans;
        let normal = spans
            .iter()
            .find(|span| span.content == "normal")
            .expect("render normal ANSI colors");
        assert_eq!(normal.style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(normal.style.bg, Some(Color::Rgb(4, 5, 6)));
        let bright = spans
            .iter()
            .find(|span| span.content == "bright")
            .expect("render bright ANSI colors");
        assert_eq!(bright.style.fg, Some(Color::Rgb(7, 8, 9)));
        assert_eq!(bright.style.bg, Some(Color::Rgb(10, 11, 12)));
    }

    #[test]
    fn bold_promotes_only_normal_ansi_foregrounds_to_bright_palette() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show bold colors".to_owned(),
            cwd: None,
            output: concat!(
                "\x1b[1;31mcolor after bold ",
                "\x1b[22mnormal after 22 ",
                "\x1b[1mbold after color ",
                "\x1b[22;91mexplicit bright\x1b[22m stays bright ",
                "\x1b[0;1;41mbold background"
            )
            .to_owned(),
            exit_status: Some(0),
        };
        let mut theme = Theme::system();
        theme.ansi.normal.red = Color::Rgb(1, 0, 0);
        theme.ansi.bright.red = Color::Rgb(2, 0, 0);
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(&mut lines, &mut links, &activity, &theme);

        let spans = &lines.last().expect("render command output").spans;
        let style_for = |content| {
            spans
                .iter()
                .find(|span| span.content == content)
                .unwrap_or_else(|| panic!("render {content:?}"))
                .style
        };
        assert_eq!(style_for("color after bold ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("normal after 22 ").fg, Some(Color::Rgb(1, 0, 0)));
        assert_eq!(style_for("bold after color ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("explicit bright").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for(" stays bright ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("bold background").bg, Some(Color::Rgb(1, 0, 0)));
    }

    #[test]
    fn transcript_cache_rebuilds_when_only_the_theme_changes() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show theme".to_owned(),
            cwd: None,
            output: "\x1b[31mthemed output".to_owned(),
            exit_status: Some(0),
        };
        let snapshot = SessionSnapshot {
            session: Session {
                id: crate::protocol::SessionId::new(),
                workspace: Workspace {
                    path: PathBuf::from("/workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: Vec::new(),
            messages: Vec::new(),
            activities: vec![activity.clone()],
            transcript: vec![TranscriptItem::Activity {
                activity_id: activity.id(),
            }],
        };
        let cache = TranscriptCache::default();
        let mut first_theme = Theme::system();
        first_theme.ansi.normal.red = Color::Rgb(1, 2, 3);
        let first = cache.view(0, &snapshot, &[], &first_theme, 80);
        let first_lines = first.window(0, 10).0;
        drop(first);
        let mut second_theme = first_theme;
        second_theme.ansi.normal.red = Color::Rgb(4, 5, 6);

        let second = cache.view(0, &snapshot, &[], &second_theme, 80);
        let second_lines = second.window(0, 10).0;

        let themed_color = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "themed output")
                .expect("render themed output")
                .style
                .fg
        };
        assert_eq!(themed_color(&first_lines), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(themed_color(&second_lines), Some(Color::Rgb(4, 5, 6)));
    }

    #[test]
    fn command_output_truncation_marker_renders_apart_from_the_output() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "emit oversized output".to_owned(),
            cwd: None,
            output: format!("\x1b[31mkept output\x1b[0m\n{TRUNCATION_MARKER}"),
            exit_status: Some(0),
        };
        let theme = Theme::system();
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(&mut lines, &mut links, &activity, &theme);

        let marker = lines.last().expect("render the truncation marker");
        assert_eq!(
            marker
                .spans
                .iter()
                .map(|span| &*span.content)
                .collect::<String>(),
            format!("    {TRUNCATION_MARKER}")
        );
        assert!(
            marker.style.add_modifier.contains(Modifier::ITALIC),
            "the marker carries a style command output cannot: {marker:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "kept output"),
            "output before the marker still renders: {lines:?}"
        );
    }

    #[test]
    fn agent_message_truncation_marker_renders_outside_the_markdown_body() {
        let message = Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: format!("```\nfenced code\n{TRUNCATION_MARKER}"),
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_message(&mut lines, &message, &theme, 80);

        let marker = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains(TRUNCATION_MARKER))
            })
            .expect("render the truncation marker");
        assert!(
            marker.style.add_modifier.contains(Modifier::ITALIC),
            "the marker keeps its own style outside the rendered Markdown: {marker:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("fenced code")),
            "Message content before the marker still renders: {lines:?}"
        );
    }

    #[test]
    fn activity_projection_retains_osc_8_link_targets() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show targets".to_owned(),
            cwd: None,
            output: concat!(
                "\x1b]8;id=first;https://example.com/first\x07first\x1b]8;;\x07 ",
                "\x1b]8;;file:///tmp/second\x1b\\second\x1b]8;;\x1b\\"
            )
            .to_owned(),
            exit_status: Some(0),
        };
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(&mut lines, &mut links, &activity, &Theme::system());

        assert_eq!(
            links
                .into_iter()
                .map(|link| link.target)
                .collect::<Vec<_>>(),
            ["https://example.com/first", "file:///tmp/second"]
        );
    }
}
