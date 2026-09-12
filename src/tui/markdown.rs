//! Practical Markdown projection for Agent-authored transcript content.
//!
//! Footnotes and a small safe HTML subset are projected deliberately. The
//! Markdown math, definition-list, superscript and subscript extensions remain
//! disabled; raw HTML super/subscript tags therefore keep only their text.

use std::collections::{HashMap, VecDeque};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};

use crate::theme::Theme;

use super::text_layout::{StyledLayout, StyledLine, StyledSpan};

pub(super) mod copy;
mod html;
mod syntax;

pub(super) fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

#[cfg(test)]
pub(super) fn render(content: &str, theme: &Theme, width: u16) -> Vec<StyledLine> {
    render_with_hyperlinks(content, theme, width, false)
}

pub(super) fn render_with_hyperlinks(
    content: &str,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    render_prose(content, theme, false, width, hyperlinks)
}

#[cfg(test)]
pub(super) fn render_reasoning(content: &str, theme: &Theme, width: u16) -> Vec<StyledLine> {
    render_reasoning_with_hyperlinks(content, theme, width, false)
}

pub(super) fn render_reasoning_with_hyperlinks(
    content: &str,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    render_prose(content, theme, true, width, hyperlinks)
}

fn render_prose(
    content: &str,
    theme: &Theme,
    subdued: bool,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    let list_plans = list_plans(content);
    let events = html::normalize(Parser::new_ext(content, options()));
    let footnotes = footnote_numbers(&events);
    let mut renderer = Renderer::new(
        theme,
        subdued,
        width,
        hyperlinks,
        list_plans.clone(),
        footnotes.clone(),
    );
    let mut builder = copy::Builder::new(list_plans, footnotes);
    for event in events {
        renderer.source = builder.event(&event, renderer.content_width());
        renderer.event(event);
    }
    let document = std::sync::Arc::new(builder.finish());
    let mut lines = renderer.finish();
    for line in &mut lines {
        line.markdown = Some(document.clone());
    }
    lines
}

fn footnote_numbers(events: &[Event<'_>]) -> HashMap<String, usize> {
    let mut numbers = HashMap::new();
    for event in events {
        if let Event::Start(Tag::FootnoteDefinition(label)) = event {
            let next = numbers.len() + 1;
            numbers.entry(label.to_string()).or_insert(next);
        }
    }
    for event in events {
        if let Event::FootnoteReference(label) = event {
            let next = numbers.len() + 1;
            numbers.entry(label.to_string()).or_insert(next);
        }
    }
    numbers
}

fn footnote_number(numbers: &HashMap<String, usize>, label: &str) -> usize {
    numbers.get(label).copied().unwrap_or(1)
}

#[derive(Clone, Copy)]
struct ListPlan {
    marker_width: usize,
    delimiter: char,
    loose: bool,
}

/// Pulldown reports an ordered list's starting number, but not its delimiter
/// or widest marker. Its source ranges retain both facts before paint begins.
fn list_plans(content: &str) -> VecDeque<ListPlan> {
    let mut plans = Vec::new();
    let mut active = Vec::new();
    let mut depth = 0_usize;
    for (event, range) in Parser::new_ext(content, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::List(first)) => {
                let delimiter = first
                    .and_then(|_| {
                        content[range]
                            .bytes()
                            .skip_while(u8::is_ascii_digit)
                            .next()
                            .map(char::from)
                    })
                    .filter(|delimiter| matches!(delimiter, '.' | ')'))
                    .unwrap_or('.');
                let index = plans.len();
                plans.push(ListPlan {
                    marker_width: 2,
                    delimiter,
                    loose: false,
                });
                active.push((index, first, 0_u64, None));
                depth = depth.saturating_add(1);
            }
            Event::Start(Tag::Item) => {
                if let Some((_, _, count, item_content_depth)) = active.last_mut() {
                    *count = count.saturating_add(1);
                    *item_content_depth = Some(depth.saturating_add(1));
                }
                depth = depth.saturating_add(1);
            }
            Event::Start(Tag::Paragraph) => {
                if let Some((index, _, _, Some(item_content_depth))) = active.last()
                    && *item_content_depth == depth
                {
                    plans[*index].loose = true;
                }
                depth = depth.saturating_add(1);
            }
            Event::End(TagEnd::List(_)) => {
                depth = depth.saturating_sub(1);
                if let Some((index, Some(first), count, _)) = active.pop() {
                    let last = first.saturating_add(count.saturating_sub(1));
                    plans[index].marker_width = last.to_string().len().saturating_add(2);
                }
            }
            Event::End(TagEnd::Item) => {
                depth = depth.saturating_sub(1);
                if let Some((_, _, _, item_content_depth)) = active.last_mut() {
                    *item_content_depth = None;
                }
            }
            Event::Start(_) => depth = depth.saturating_add(1),
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    plans.into()
}

struct ListState {
    next: Option<u64>,
    marker_width: usize,
    delimiter: char,
    loose: bool,
}

struct ItemState {
    /// Present until the item's first content line is emitted; its width stays
    /// behind as the continuation indent for every later line.
    marker: Option<StyledSpan>,
    marker_width: usize,
    quote_depth: usize,
}

struct Renderer<'a> {
    theme: &'a Theme,
    subdued: bool,
    width: u16,
    hyperlinks: bool,
    lines: Vec<StyledLine>,
    current: Vec<StyledSpan>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    list_plans: VecDeque<ListPlan>,
    items: Vec<ItemState>,
    /// Destinations currently enclosing paint. `true` identifies an anchor;
    /// an Image nested inside one remains actionable through the outer Link.
    links: Vec<(String, bool)>,
    code_block: Option<CodeBlock>,
    source: Option<copy::SourceRange>,
    table: Option<Table>,
    quote_depth: usize,
    footnote_numbers: HashMap<String, usize>,
    footnote_lines: Vec<StyledLine>,
    in_footnote: bool,
}

struct CodeBlock {
    info: String,
    content: String,
    source: Option<copy::SourceRange>,
}

#[derive(Default)]
struct Table {
    alignments: Vec<copy::ColumnAlignment>,
    rows: Vec<TableRow>,
    current_row: Option<TableRow>,
    current_cell: Option<TableCell>,
}

#[derive(Default)]
struct TableRow {
    header: bool,
    cells: Vec<TableCell>,
}

#[derive(Default)]
struct TableCell {
    spans: Vec<StyledSpan>,
    boundary: Option<copy::SourceRange>,
}

impl<'a> Renderer<'a> {
    fn new(
        theme: &'a Theme,
        subdued: bool,
        width: u16,
        hyperlinks: bool,
        list_plans: VecDeque<ListPlan>,
        footnote_numbers: HashMap<String, usize>,
    ) -> Self {
        Self {
            theme,
            subdued,
            width,
            hyperlinks,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![theme.markdown.text],
            lists: Vec::new(),
            list_plans,
            items: Vec::new(),
            links: Vec::new(),
            code_block: None,
            source: None,
            table: None,
            quote_depth: 0,
            footnote_numbers,
            footnote_lines: Vec::new(),
            in_footnote: false,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(text.as_ref()),
            Event::Code(code) => self.push(
                code.as_ref(),
                self.current_style().patch(self.theme.markdown.inline_code),
            ),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.push(math.as_ref(), self.current_style())
            }
            Event::Html(html) | Event::InlineHtml(html) => self.text(html.as_ref()),
            Event::FootnoteReference(label) => {
                let number = footnote_number(&self.footnote_numbers, label.as_ref());
                self.push(format!("[{number}]"), self.theme.text.subdued);
            }
            Event::SoftBreak => {
                if let Some(block) = &mut self.code_block {
                    block.content.push('\n');
                } else {
                    self.push(" ", self.current_style());
                }
            }
            Event::HardBreak => {
                if self.table.is_some() {
                    self.push(" ", self.current_style());
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                self.flush_line();
                self.push_chrome(
                    "─".repeat(usize::from(self.content_width())),
                    self.theme.markdown.rule,
                );
                self.flush_line();
                self.blank_line();
            }
            Event::TaskListMarker(checked) => self.task_marker(checked),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                let spaced_before = matches!(level, HeadingLevel::H1 | HeadingLevel::H2);
                if self.has_pending_list_marker() {
                    if spaced_before {
                        self.blank_line_before_block();
                    }
                } else if spaced_before {
                    self.blank_line_before_block();
                } else {
                    self.flush_line();
                }
                self.push_style(self.heading_style(level));
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.code_block = Some(CodeBlock {
                    info: match &kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    },
                    content: String::new(),
                    source: None,
                });
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    // The label names the fence rather than belonging to the
                    // code, so a copy of the Code Block leaves it out.
                    self.push_chrome(language.as_ref(), self.theme.text.subdued);
                    self.flush_line();
                }
            }
            Tag::List(first) => {
                if self.has_pending_list_marker() {
                    if self.current.is_empty() {
                        self.emit_line(StyledLine::default());
                    } else {
                        self.flush_line();
                    }
                }
                let plan = self.list_plans.pop_front().unwrap_or(ListPlan {
                    marker_width: 2,
                    delimiter: '.',
                    loose: false,
                });
                self.lists.push(ListState {
                    next: first,
                    marker_width: plan.marker_width,
                    delimiter: plan.delimiter,
                    loose: plan.loose,
                });
            }
            Tag::Item => {
                self.flush_line();
                let (marker, marker_width, marker_style) = match self.lists.last_mut() {
                    Some(ListState {
                        next: Some(next),
                        marker_width,
                        delimiter,
                        ..
                    }) => {
                        let ordinal = format!("{next}{delimiter} ");
                        *next = next.saturating_add(1);
                        (
                            format!(
                                "{}{ordinal}",
                                " ".repeat(marker_width.saturating_sub(ordinal.len()))
                            ),
                            *marker_width,
                            self.theme.markdown.list_enumeration,
                        )
                    }
                    Some(list) => (
                        "• ".to_owned(),
                        list.marker_width,
                        self.theme.markdown.list_marker,
                    ),
                    None => ("• ".to_owned(), 2, self.theme.markdown.list_marker),
                };
                self.items.push(ItemState {
                    marker: Some(StyledSpan::chrome(marker, self.prose_style(marker_style))),
                    marker_width,
                    quote_depth: self.quote_depth,
                });
            }
            Tag::Emphasis => self.push_style(self.theme.markdown.emphasis),
            Tag::Strong => self.push_style(self.theme.markdown.strong),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.into_string(), true));
                self.push_style(self.theme.markdown.link_text);
            }
            Tag::Image { dest_url, .. } => {
                self.push_chrome(
                    "[image: ",
                    self.current_style().patch(self.theme.markdown.image),
                );
                self.links.push((dest_url.into_string(), false));
                self.push_style(self.theme.markdown.image_text);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(Table {
                    alignments: alignments
                        .into_iter()
                        .map(copy::ColumnAlignment::from)
                        .collect(),
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.current_row = Some(TableRow {
                        header: true,
                        cells: Vec::new(),
                    });
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.current_row = Some(TableRow {
                        header: false,
                        cells: Vec::new(),
                    });
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.current_cell = Some(TableCell::default());
                }
            }
            Tag::HtmlBlock => {}
            Tag::FootnoteDefinition(label) => {
                self.flush_line();
                self.in_footnote = true;
                let number = footnote_number(&self.footnote_numbers, label.as_ref());
                let marker = format!("  [{number}] ");
                let marker_width = marker.len();
                self.items.push(ItemState {
                    marker: Some(StyledSpan::chrome(
                        marker,
                        self.prose_style(self.theme.text.subdued),
                    )),
                    marker_width,
                    quote_depth: self.quote_depth,
                });
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank_line();
            }
            TagEnd::Heading(level) => {
                self.pop_style();
                self.flush_line();
                if level == HeadingLevel::H1 {
                    self.blank_line();
                }
            }
            TagEnd::CodeBlock => {
                if let Some(block) = self.code_block.take() {
                    let mut offset = 0;
                    let flat_style = self.prose_style(self.theme.markdown.code_block);
                    let lines = if self.subdued {
                        syntax::flat_lines(&block.content, flat_style)
                    } else {
                        syntax::render(&block.content, &block.info, self.theme, flat_style)
                    }
                    .into_iter()
                    .map(|line| {
                        let mut line = StyledLine::from(line);
                        for span in &mut line.spans {
                            span.source = block
                                .source
                                .as_ref()
                                .map(|source| source.slice(offset..offset + span.content.len()));
                            offset += span.content.len();
                        }
                        offset += 1;
                        line
                    })
                    .collect::<Vec<_>>();
                    for line in lines {
                        self.emit_line(line);
                    }
                }
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.remove_trailing_separator();
                self.lists.pop();
                if self
                    .lists
                    .last()
                    .is_none_or(|parent_list| parent_list.loose)
                {
                    self.blank_line();
                }
            }
            TagEnd::Item => {
                if self.current.is_empty() && self.has_pending_list_marker() {
                    self.emit_line(StyledLine::default());
                } else {
                    self.flush_line();
                }
                self.items.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => self.close_destination(self.theme.markdown.link, None),
            TagEnd::Image => self.close_destination(self.theme.markdown.image, Some("]")),
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.remove_trailing_separator();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(mut cell) = table.current_cell.take()
                {
                    cell.boundary = self.source.clone();
                    if let Some(row) = &mut table.current_row {
                        row.cells.push(cell);
                    }
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table
                    && let Some(row) = table.current_row.take()
                {
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let mut lines = table.render(self.content_width(), self.theme);
                    if self.subdued {
                        for span in lines.iter_mut().flat_map(|line| &mut line.spans) {
                            span.style = self.prose_style(span.style);
                        }
                    }
                    for line in lines {
                        self.emit_line(line);
                    }
                }
                self.blank_line();
            }
            TagEnd::HtmlBlock => {}
            TagEnd::FootnoteDefinition => {
                self.flush_line();
                self.items.pop();
                self.in_footnote = false;
            }
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(block) = &mut self.code_block {
            if block.source.is_none() {
                block.source = self.source.clone();
            }
            block.content.push_str(text);
        } else {
            self.push(text, self.current_style());
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.current_style().patch(style));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn close_destination(&mut self, destination_style: Style, closing_chrome: Option<&str>) {
        self.pop_style();
        let destination_style = self.current_style().patch(destination_style);
        if let Some(closing_chrome) = closing_chrome {
            self.push_chrome(closing_chrome, destination_style);
        }
        if let Some((destination, _)) = self.links.pop() {
            let target = super::clipboard::safe_hyperlink_target(&destination);
            if !self.hyperlinks || target.is_none() {
                let visible = destination
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect::<String>();
                let mut span = StyledSpan::chrome(
                    format!(" ({})", visible.trim()),
                    self.prose_style(destination_style),
                )
                .with_target(target);
                span.source = self.source.clone();
                self.push_span(span);
            }
        }
    }

    fn current_style(&self) -> Style {
        *self
            .styles
            .last()
            .expect("Markdown style stack is non-empty")
    }

    fn prose_style(&self, style: Style) -> Style {
        if self.subdued {
            self.theme.text.subdued.add_modifier(style.add_modifier)
        } else {
            style
        }
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        let modifiers = match level {
            HeadingLevel::H1 => Modifier::BOLD | Modifier::UNDERLINED,
            HeadingLevel::H2 | HeadingLevel::H3 => Modifier::BOLD,
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
                Modifier::BOLD | Modifier::DIM
            }
        };
        self.theme.markdown.heading.add_modifier(modifiers)
    }

    fn push(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            self.push_span(StyledSpan::text(content, self.prose_style(style)));
        }
    }

    fn push_chrome(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            self.push_span(StyledSpan::chrome(content, self.prose_style(style)));
        }
    }

    fn task_marker(&mut self, checked: bool) {
        let marker = if checked { "[x] " } else { "[ ] " };
        let style = self.prose_style(self.theme.markdown.list_marker);
        if let Some(item) = self.items.last_mut()
            && item.marker.is_some()
        {
            item.marker_width = item.marker_width.max(marker.len());
            item.marker = Some(StyledSpan::chrome(
                format!(
                    "{}{}",
                    " ".repeat(item.marker_width.saturating_sub(marker.len())),
                    marker
                ),
                style,
            ));
        } else {
            self.push_chrome(marker, self.theme.markdown.list_marker);
        }
    }

    fn push_span(&mut self, mut span: StyledSpan) {
        span.source = self.source.clone();
        if span.target.is_none()
            && let Some(target) = self
                .links
                .iter()
                .rev()
                .find(|(_, anchor)| *anchor)
                .or_else(|| self.links.last())
                .and_then(|(target, _)| super::clipboard::safe_hyperlink_target(target))
        {
            span.target = Some(target.to_owned());
        }
        if let Some(cell) = self
            .table
            .as_mut()
            .and_then(|table| table.current_cell.as_mut())
        {
            cell.spans.push(span);
        } else {
            self.current.push(span);
        }
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            let line = StyledLine::from(std::mem::take(&mut self.current));
            self.emit_line(line);
        }
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if !self
            .output_lines()
            .last()
            .is_some_and(|line| self.is_separator(line))
        {
            self.emit_separator();
        }
    }

    fn blank_line_before_block(&mut self) {
        self.flush_line();
        if !self.output_lines().is_empty() {
            self.blank_line();
        }
    }

    fn emit_line(&mut self, mut line: StyledLine) {
        let prefix = self.structural_prefix(true);
        line.spans.splice(0..0, prefix);
        self.output_lines_mut().push(line);
    }

    fn emit_separator(&mut self) {
        let mut line = StyledLine::default();
        line.spans = self.structural_prefix(false);
        self.output_lines_mut().push(line);
    }

    fn structural_prefix(&mut self, content: bool) -> Vec<StyledSpan> {
        let indent_style = self.prose_style(self.theme.markdown.text);
        let quote_style = self.prose_style(self.theme.markdown.block_quote);
        let mut prefix = Vec::new();
        // Quote depth records where an Item began so nested containers retain
        // source order: outer quote, parent Item indent, then inner quote.
        for depth in 0..=self.quote_depth {
            for item in self
                .items
                .iter_mut()
                .filter(|item| item.quote_depth == depth)
            {
                prefix.push(if content {
                    item.marker.take().unwrap_or_else(|| {
                        StyledSpan::chrome(" ".repeat(item.marker_width), indent_style)
                    })
                } else {
                    StyledSpan::chrome(" ".repeat(item.marker_width), indent_style)
                });
            }
            if depth < self.quote_depth {
                prefix.push(StyledSpan::chrome("│ ", quote_style));
            }
        }
        prefix
    }

    fn has_pending_list_marker(&self) -> bool {
        self.items.last().is_some_and(|item| item.marker.is_some())
    }

    fn is_separator(&self, line: &StyledLine) -> bool {
        line.spans.iter().all(|span| {
            span.chrome && (span.content == "│ " || span.content.chars().all(char::is_whitespace))
        })
    }

    fn remove_trailing_separator(&mut self) {
        if self
            .output_lines()
            .last()
            .is_some_and(|line| self.is_separator(line))
        {
            self.output_lines_mut().pop();
        }
    }

    fn output_lines(&self) -> &[StyledLine] {
        if self.in_footnote {
            &self.footnote_lines
        } else {
            &self.lines
        }
    }

    fn output_lines_mut(&mut self) -> &mut Vec<StyledLine> {
        if self.in_footnote {
            &mut self.footnote_lines
        } else {
            &mut self.lines
        }
    }

    fn content_width(&self) -> u16 {
        let prefix = self
            .items
            .iter()
            .map(|item| item.marker_width)
            .sum::<usize>()
            .saturating_add(self.quote_depth.saturating_mul(2));
        let prefix = u16::try_from(prefix).unwrap_or(u16::MAX);
        self.width.saturating_sub(prefix)
    }

    fn finish(mut self) -> Vec<StyledLine> {
        self.flush_line();
        while self
            .lines
            .last()
            .is_some_and(|line| self.is_separator(line))
        {
            self.lines.pop();
        }
        while self
            .footnote_lines
            .last()
            .is_some_and(|line| self.is_separator(line))
        {
            self.footnote_lines.pop();
        }
        if !self.footnote_lines.is_empty() {
            let mut rule = StyledLine::default();
            rule.spans.push(StyledSpan::chrome(
                "─".repeat(usize::from(self.width)),
                self.prose_style(self.theme.markdown.rule),
            ));
            self.lines.push(rule);
            self.lines.append(&mut self.footnote_lines);
        }
        self.lines
    }
}

const MIN_TABLE_COLUMN_WIDTH: usize = 3;

impl TableCell {
    fn width(&self) -> usize {
        self.spans.iter().map(StyledSpan::width).sum()
    }

    fn line(&self, header: bool, theme: &Theme) -> StyledLine {
        let mut spans = self.spans.clone();
        if header {
            for span in &mut spans {
                span.style = span.style.patch(theme.markdown.strong);
            }
        }
        StyledLine::from(spans)
    }
}

impl Table {
    fn render(self, width: u16, theme: &Theme) -> Vec<StyledLine> {
        let columns = self
            .rows
            .iter()
            .map(|row| row.cells.len())
            .chain(std::iter::once(self.alignments.len()))
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return Vec::new();
        }
        let natural = (0..columns)
            .map(|column| {
                self.rows
                    .iter()
                    .filter_map(|row| row.cells.get(column))
                    .map(TableCell::width)
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect::<Vec<_>>();
        let minimum = natural
            .iter()
            .map(|width| (*width).min(MIN_TABLE_COLUMN_WIDTH))
            .collect::<Vec<_>>();
        let available = usize::from(width);
        if table_width(&minimum) > available {
            return self.render_pipe(columns, theme);
        }
        let widths = fit_table_widths(&natural, &minimum, available);
        self.render_bordered(&widths, theme)
    }

    fn render_bordered(self, widths: &[usize], theme: &Theme) -> Vec<StyledLine> {
        let mut lines = vec![table_rule('┌', '┬', '┐', widths, theme)];
        for row in &self.rows {
            lines.extend(self.render_row(row, widths, theme));
            if row.header {
                lines.push(table_rule('├', '┼', '┤', widths, theme));
            }
        }
        lines.push(table_rule('└', '┴', '┘', widths, theme));
        lines
    }

    fn render_row(&self, row: &TableRow, widths: &[usize], theme: &Theme) -> Vec<StyledLine> {
        let cells = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                row.cells.get(column).map_or_else(
                    || vec![Vec::new()],
                    |cell| wrapped_cell(cell, *width, row.header, theme),
                )
            })
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|line_index| {
                let mut spans = vec![StyledSpan::chrome("│", theme.border.subdued)];
                for (column, width) in widths.iter().copied().enumerate() {
                    let content = cells[column].get(line_index).cloned().unwrap_or_default();
                    let content_width = content.iter().map(StyledSpan::width).sum::<usize>();
                    let remaining = width.saturating_sub(content_width);
                    let alignment = self
                        .alignments
                        .get(column)
                        .copied()
                        .unwrap_or(copy::ColumnAlignment::None);
                    let (leading, trailing) = alignment_padding(alignment, remaining);
                    let padding_style = if row.header {
                        theme.markdown.text.patch(theme.markdown.strong)
                    } else {
                        theme.markdown.text
                    };
                    spans.push(StyledSpan::chrome(" ", padding_style));
                    push_padding(&mut spans, leading, padding_style);
                    spans.extend(content);
                    push_padding(&mut spans, trailing, padding_style);
                    let mut boundary = StyledSpan::chrome(" ", padding_style);
                    if line_index + 1 == cells[column].len() {
                        // A selected empty cell still needs one painted byte
                        // that names its copy node. Keep that witness no longer
                        // than the one byte mapped here; the builder's complete
                        // `" | "` range must never describe wider padding.
                        boundary.source = row
                            .cells
                            .get(column)
                            .and_then(|cell| cell.boundary.as_ref())
                            .map(|source| source.slice(0..1));
                    }
                    spans.push(boundary);
                    spans.push(StyledSpan::chrome("│", theme.border.subdued));
                }
                StyledLine::from(spans)
            })
            .collect()
    }

    fn render_pipe(self, columns: usize, theme: &Theme) -> Vec<StyledLine> {
        let mut lines = Vec::new();
        for row in &self.rows {
            lines.push(pipe_row(row, columns, theme));
            if row.header {
                let separator = (0..columns)
                    .map(|column| {
                        self.alignments
                            .get(column)
                            .unwrap_or(&copy::ColumnAlignment::None)
                            .separator()
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                lines.push(StyledLine::chrome(
                    format!("| {separator} |"),
                    theme.border.subdued,
                ));
            }
        }
        lines
    }
}

fn table_width(widths: &[usize]) -> usize {
    widths
        .iter()
        .sum::<usize>()
        .saturating_add(widths.len().saturating_mul(3))
        .saturating_add(1)
}

fn fit_table_widths(natural: &[usize], minimum: &[usize], available: usize) -> Vec<usize> {
    let overhead = natural.len().saturating_mul(3).saturating_add(1);
    let content_budget = available.saturating_sub(overhead);
    let mut low = 0;
    let mut high = natural.iter().copied().max().unwrap_or(0);
    while low < high {
        let cap = low + (high - low).div_ceil(2);
        let used = natural
            .iter()
            .zip(minimum)
            .map(|(natural, minimum)| (*natural).min(cap).max(*minimum))
            .sum::<usize>();
        if used <= content_budget {
            low = cap;
        } else {
            high = cap - 1;
        }
    }
    let mut widths = natural
        .iter()
        .zip(minimum)
        .map(|(natural, minimum)| (*natural).min(low).max(*minimum))
        .collect::<Vec<_>>();
    let mut remaining = content_budget.saturating_sub(widths.iter().sum());
    for (width, natural) in widths.iter_mut().zip(natural) {
        if remaining == 0 {
            break;
        }
        if *width < *natural {
            *width += 1;
            remaining -= 1;
        }
    }
    widths
}

fn table_rule(
    left: char,
    junction: char,
    right: char,
    widths: &[usize],
    theme: &Theme,
) -> StyledLine {
    let mut rule = left.to_string();
    rule.push_str(
        &widths
            .iter()
            .map(|width| "─".repeat(width.saturating_add(2)))
            .collect::<Vec<_>>()
            .join(&junction.to_string()),
    );
    rule.push(right);
    StyledLine::chrome(rule, theme.border.subdued)
}

fn wrapped_cell(
    cell: &TableCell,
    width: usize,
    header: bool,
    theme: &Theme,
) -> Vec<Vec<StyledSpan>> {
    let line = cell.line(header, theme);
    if line.spans.is_empty() {
        return vec![Vec::new()];
    }
    StyledLayout::new(&line, u16::try_from(width).unwrap_or(u16::MAX))
        .rows()
        .iter()
        .map(|row| {
            let mut spans = Vec::new();
            if row.indent > 0 {
                spans.push(StyledSpan::chrome(
                    " ".repeat(row.indent),
                    theme.markdown.text,
                ));
            }
            spans.extend(line.slice(row.start..row.end).spans);
            spans
        })
        .collect()
}

fn alignment_padding(alignment: copy::ColumnAlignment, space: usize) -> (usize, usize) {
    match alignment {
        copy::ColumnAlignment::None | copy::ColumnAlignment::Left => (0, space),
        copy::ColumnAlignment::Center => (space / 2, space - space / 2),
        copy::ColumnAlignment::Right => (space, 0),
    }
}

fn push_padding(spans: &mut Vec<StyledSpan>, width: usize, style: Style) {
    if width > 0 {
        spans.push(StyledSpan::chrome(" ".repeat(width), style));
    }
}

fn pipe_row(row: &TableRow, columns: usize, theme: &Theme) -> StyledLine {
    let mut spans = vec![StyledSpan::chrome("| ", theme.border.subdued)];
    for column in 0..columns {
        if let Some(cell) = row.cells.get(column) {
            spans.extend(cell.line(row.header, theme).spans);
        }
        let mut boundary = StyledSpan::chrome(" | ", theme.border.subdued);
        boundary.source = row.cells.get(column).and_then(|cell| cell.boundary.clone());
        spans.push(boundary);
    }
    StyledLine::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    use crate::tui::text_layout::StyledLayout;

    // These tests assert paint, independently of the source metadata exercised
    // through rendered Application selections in the integration suite.
    fn painted(mut lines: Vec<StyledLine>) -> Vec<StyledLine> {
        for line in &mut lines {
            line.markdown = None;
            for span in &mut line.spans {
                span.source = None;
                span.target = None;
            }
        }
        lines
    }

    fn render(content: &str, theme: &Theme) -> Vec<StyledLine> {
        painted(super::render(content, theme, 80))
    }

    fn render_reasoning(content: &str, theme: &Theme) -> Vec<StyledLine> {
        painted(super::render_reasoning(content, theme, 80))
    }

    fn line_texts(lines: &[StyledLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn each_heading_level_paints_its_distinguishing_modifiers() {
        let theme = Theme::system();
        let distinguishing = Modifier::BOLD | Modifier::UNDERLINED | Modifier::DIM;

        for (markdown, expected) in [
            ("# H1", Modifier::BOLD | Modifier::UNDERLINED),
            ("## H2", Modifier::BOLD),
            ("### H3", Modifier::BOLD),
            ("#### H4", Modifier::BOLD | Modifier::DIM),
            ("##### H5", Modifier::BOLD | Modifier::DIM),
            ("###### H6", Modifier::BOLD | Modifier::DIM),
        ] {
            let lines = render(markdown, &theme);
            let heading = &lines[0].spans[0];

            assert!(!heading.chrome);
            assert_eq!(heading.style.fg, theme.markdown.heading.fg);
            assert_eq!(heading.style.add_modifier & distinguishing, expected);
        }
    }

    #[test]
    fn heading_modifiers_compose_with_nested_inline_emphasis() {
        let theme = Theme::system();

        let lines = render("#### faded *soft* and **strong**", &theme);
        let soft = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "soft")
            .expect("emphasized heading text remains a distinct span");
        let strong = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "strong")
            .expect("strong heading text remains a distinct span");

        assert!(soft.style.add_modifier.contains(Modifier::BOLD));
        assert!(soft.style.add_modifier.contains(Modifier::DIM));
        assert!(soft.style.add_modifier.contains(Modifier::ITALIC));
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
        assert!(strong.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn heading_levels_apply_only_their_decided_block_spacing() {
        let theme = Theme::system();
        let lines = render(
            "### before h2\n## h2\n### after h2\n# h1\n#### after h1",
            &theme,
        );
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            text,
            ["before h2", "", "h2", "after h2", "", "h1", "", "after h1",]
        );
    }

    #[test]
    fn a_leading_h1_or_h2_does_not_create_a_blank_message_row() {
        let theme = Theme::system();

        for heading in ["# first", "## first"] {
            let lines = render(heading, &theme);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].spans[0].content, "first");
        }
    }

    #[test]
    fn every_heading_level_stays_on_its_list_marker_line() {
        let theme = Theme::system();

        for (markdown, title) in [
            ("- # H1", "H1"),
            ("- ## H2", "H2"),
            ("- ### H3", "H3"),
            ("- #### H4", "H4"),
            ("- ##### H5", "H5"),
            ("- ###### H6", "H6"),
        ] {
            let lines = render(markdown, &theme);
            assert_eq!(lines.len(), 1, "{markdown}");
            assert_eq!(lines[0].spans[0].content, "• ", "{markdown}");
            assert_eq!(lines[0].spans[1].content, title, "{markdown}");
        }
    }

    #[test]
    fn h1_and_h2_spacing_precedes_a_pending_list_marker() {
        let theme = Theme::system();
        let lines = render(
            "- ### before h2\n- ## h2\n- #### after h2\n- # h1\n- ##### after h1",
            &theme,
        );
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            text,
            [
                "• before h2",
                "  ",
                "• h2",
                "• after h2",
                "  ",
                "• h1",
                "  ",
                "• after h1",
            ]
        );
    }

    #[test]
    fn heading_in_a_quoted_list_keeps_all_structural_prefixes_on_one_line() {
        let theme = Theme::system();

        let lines = render("> - ## Title", &theme);

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            StyledLine::from(vec![
                StyledSpan::chrome("│ ", theme.markdown.block_quote),
                StyledSpan::chrome("• ", theme.markdown.list_marker),
                StyledSpan::text("Title", theme.markdown.heading),
            ])
        );
    }

    #[test]
    fn lists_paint_markers_as_chrome_and_use_markdown_text_for_the_item_body() {
        let theme = Theme::system();

        let lines = render("- first\n- second", &theme);

        assert_eq!(
            lines,
            vec![
                StyledLine::from(vec![
                    StyledSpan::chrome("• ", theme.markdown.list_marker),
                    StyledSpan::text("first", theme.markdown.text),
                ]),
                StyledLine::from(vec![
                    StyledSpan::chrome("• ", theme.markdown.list_marker),
                    StyledSpan::text("second", theme.markdown.text),
                ]),
            ]
        );
        assert!(lines.iter().all(|line| line.spans[0].chrome));
        assert!(lines.iter().all(|line| !line.spans[1].chrome));
    }

    #[test]
    fn nested_item_continuation_paragraphs_keep_all_ancestor_marker_widths() {
        let theme = Theme::system();
        let lines = render("- outer\n  - nested b\n\n    continuation", &theme);

        assert_eq!(
            line_texts(&lines),
            ["• outer", "  • nested b", "    ", "    continuation"]
        );
    }

    #[test]
    fn an_item_whose_only_content_is_a_nested_list_keeps_markers_on_separate_lines() {
        let theme = Theme::system();
        let lines = render("- - inner", &theme);

        assert_eq!(line_texts(&lines), ["• ", "  • inner"]);
    }

    #[test]
    fn tight_and_loose_lists_have_distinct_item_spacing() {
        let theme = Theme::system();

        let tight = render("- first\n- second", &theme);
        let loose = render("- first\n\n- second", &theme);

        assert_eq!(line_texts(&tight), ["• first", "• second"]);
        assert_eq!(line_texts(&loose), ["• first", "  ", "• second"]);
    }

    #[test]
    fn a_loose_nested_list_does_not_add_spacing_to_its_tight_parent() {
        let theme = Theme::system();
        let lines = render("- outer\n  - inner one\n\n  - inner two\n- sibling", &theme);

        assert_eq!(
            line_texts(&lines),
            [
                "• outer",
                "  • inner one",
                "    ",
                "  • inner two",
                "• sibling",
            ]
        );
    }

    #[test]
    fn a_nested_list_keeps_the_loose_parent_boundary_before_its_sibling() {
        let theme = Theme::system();
        let lines = render("- outer\n\n  - inner\n\n- sibling", &theme);

        assert_eq!(
            line_texts(&lines),
            ["• outer", "  ", "  • inner", "  ", "• sibling"]
        );
    }

    #[test]
    fn a_nested_list_keeps_the_loose_parent_boundary_before_a_continuation() {
        let theme = Theme::system();
        let lines = render("- outer\n  - inner\n\n  continuation", &theme);

        assert_eq!(
            line_texts(&lines),
            ["• outer", "  ", "  • inner", "  ", "  continuation"]
        );
    }

    #[test]
    fn twelve_ordered_items_right_align_to_the_widest_marker() {
        let theme = Theme::system();
        let markdown = (1..=12)
            .map(|number| format!("{number}. item {number}"))
            .collect::<Vec<_>>()
            .join("\n");

        let lines = render(&markdown, &theme);

        assert_eq!(lines.len(), 12);
        assert_eq!(line_texts(&lines)[0], " 1. item 1");
        assert_eq!(line_texts(&lines)[8], " 9. item 9");
        assert_eq!(line_texts(&lines)[9], "10. item 10");
        assert_eq!(line_texts(&lines)[11], "12. item 12");
    }

    #[test]
    fn task_marker_width_and_list_quote_order_flow_into_nested_lines() {
        let theme = Theme::system();
        let task = render("- [x] parent\n  - child", &theme);
        let ordered = render("10. parent\n    - child", &theme);
        let quote = render("> - item\n>   > nested quote", &theme);

        assert_eq!(line_texts(&task), ["[x] parent", "    • child"]);
        assert_eq!(line_texts(&ordered), ["10. parent", "    • child"]);
        assert_eq!(line_texts(&quote), ["│ • item", "│   │ nested quote"]);
    }

    #[test]
    fn parent_marker_width_prefixes_code_and_reserves_table_width() {
        let theme = Theme::system();
        let code = render("- ```rust\n  let answer = 42;\n  ```", &theme);
        assert_eq!(line_texts(&code), ["• rust", "  let answer = 42;"]);

        let table = super::render(
            "- table:\n\n  | first-column | second |\n  | --- | --- |\n  | alpha beta gamma | value |",
            &theme,
            28,
        );
        let table_lines = table
            .iter()
            .filter(|line| {
                line.spans.iter().any(|span| span.content.contains('┌'))
                    || line.spans.iter().any(|span| span.content.contains('│'))
                    || line.spans.iter().any(|span| span.content.contains('└'))
            })
            .collect::<Vec<_>>();
        assert!(!table_lines.is_empty());
        assert!(table_lines.iter().all(|line| line.width() <= 28));
        assert!(table_lines.iter().all(|line| line.spans[0].content == "  "));
    }

    #[test]
    fn parenthesized_ordinals_wrap_beneath_their_text() {
        let theme = Theme::system();
        let lines = super::render(
            "9) first item that wraps onto another row\n10) second",
            &theme,
            20,
        );
        let layout = StyledLayout::new(&lines[0], 20);

        assert_eq!(
            line_texts(&lines)[0],
            " 9) first item that wraps onto another row"
        );
        assert_eq!(line_texts(&lines)[1], "10) second");
        assert_eq!(
            layout
                .rows()
                .iter()
                .map(|row| {
                    (
                        row.indent,
                        row.line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (0, " 9) first item that".into()),
                (4, "    wraps onto".into()),
                (4, "    another row".into()),
            ]
        );
    }

    #[test]
    fn task_items_replace_bullets_with_ascii_checkbox_chrome() {
        let theme = Theme::system();

        let lines = render("- [ ] pending\n- [x] done", &theme);

        assert_eq!(
            lines,
            vec![
                StyledLine::from(vec![
                    StyledSpan::chrome("[ ] ", theme.markdown.list_marker),
                    StyledSpan::text("pending", theme.markdown.text),
                ]),
                StyledLine::from(vec![
                    StyledSpan::chrome("[x] ", theme.markdown.list_marker),
                    StyledSpan::text("done", theme.markdown.text),
                ]),
            ]
        );
    }

    #[test]
    fn wrapped_task_text_hangs_beneath_the_checkbox() {
        let theme = Theme::system();
        let lines = super::render("- [x] first task that wraps onto another row", &theme, 20);

        let layout = StyledLayout::new(&lines[0], 20);
        let rows = layout.rows();
        assert_eq!(
            rows.iter()
                .map(|row| {
                    (
                        row.indent,
                        row.line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (0, "[x] first task that".into()),
                (4, "    wraps onto".into()),
                (4, "    another row".into()),
            ]
        );
    }

    #[test]
    fn quotes_paint_the_bar_as_chrome_and_the_words_as_text() {
        let theme = Theme::system();

        let lines = render("> quoted words", &theme);

        assert_eq!(
            lines,
            vec![StyledLine::from(vec![
                StyledSpan::chrome("│ ", theme.markdown.block_quote),
                StyledSpan::text("quoted words", theme.markdown.text),
            ])]
        );
        assert!(lines[0].spans[0].chrome);
        assert!(!lines[0].spans[1].chrome);
    }

    #[test]
    fn nested_quotes_prefix_each_line_at_its_depth() {
        let theme = Theme::system();

        let lines = render("> > nested words", &theme);

        assert_eq!(
            lines,
            vec![StyledLine::from(vec![
                StyledSpan::chrome("│ ", theme.markdown.block_quote),
                StyledSpan::chrome("│ ", theme.markdown.block_quote),
                StyledSpan::text("nested words", theme.markdown.text),
            ])]
        );
    }

    #[test]
    fn quote_prefixes_both_list_items_without_dangling_lines() {
        let theme = Theme::system();

        let lines = render("> - first\n> - second", &theme);

        assert_eq!(
            lines,
            vec![
                StyledLine::from(vec![
                    StyledSpan::chrome("│ ", theme.markdown.block_quote),
                    StyledSpan::chrome("• ", theme.markdown.list_marker),
                    StyledSpan::text("first", theme.markdown.text),
                ]),
                StyledLine::from(vec![
                    StyledSpan::chrome("│ ", theme.markdown.block_quote),
                    StyledSpan::chrome("• ", theme.markdown.list_marker),
                    StyledSpan::text("second", theme.markdown.text),
                ]),
            ]
        );
    }

    #[test]
    fn quote_prefixes_fence_labels_and_every_code_line() {
        let theme = Theme::system();

        let lines = render("> ```rust\n> let answer = 42;\n> ```", &theme);
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, ["│ rust", "│ let answer = 42;"]);
        assert!(lines.iter().all(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.chrome && span.content == "│ ")
        }));
    }

    #[test]
    fn quote_keeps_its_bar_on_authored_paragraph_separators() {
        let theme = Theme::system();

        let lines = render("> first\n>\n> second", &theme);
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, ["│ first", "│ ", "│ second"]);
        assert!(lines[1].spans[0].chrome);
    }

    #[test]
    fn nested_quote_tables_fit_after_their_prefix_width_is_reserved() {
        let theme = Theme::system();
        let markdown =
            "> > | first-column | second |\n> > | --- | --- |\n> > | alpha beta gamma | value |";

        let lines = super::render(markdown, &theme, 28);

        assert!(lines.len() > 3, "the table keeps its bordered projection");
        assert!(lines.iter().all(|line| line.width() <= 28));
        assert!(lines.iter().all(|line| {
            line.spans.get(0).is_some_and(|span| span.content == "│ ")
                && line.spans.get(1).is_some_and(|span| span.content == "│ ")
        }));
    }

    #[test]
    fn tables_paint_light_borders_and_padding_as_chrome() {
        let theme = Theme::system();
        let header = theme.markdown.text.patch(theme.markdown.strong);

        let lines = render("| Name | Value |\n| :--- | ---: |\n| alpha | 42 |", &theme);

        assert_eq!(
            lines,
            vec![
                StyledLine::chrome("┌───────┬───────┐", theme.border.subdued),
                StyledLine::from(vec![
                    StyledSpan::chrome("│", theme.border.subdued),
                    StyledSpan::chrome(" ", header),
                    StyledSpan::text("Name", header),
                    StyledSpan::chrome(" ", header),
                    StyledSpan::chrome(" ", header),
                    StyledSpan::chrome("│", theme.border.subdued),
                    StyledSpan::chrome(" ", header),
                    StyledSpan::text("Value", header),
                    StyledSpan::chrome(" ", header),
                    StyledSpan::chrome("│", theme.border.subdued),
                ]),
                StyledLine::chrome("├───────┼───────┤", theme.border.subdued),
                StyledLine::from(vec![
                    StyledSpan::chrome("│", theme.border.subdued),
                    StyledSpan::chrome(" ", theme.markdown.text),
                    StyledSpan::text("alpha", theme.markdown.text),
                    StyledSpan::chrome(" ", theme.markdown.text),
                    StyledSpan::chrome("│", theme.border.subdued),
                    StyledSpan::chrome(" ", theme.markdown.text),
                    StyledSpan::chrome("   ", theme.markdown.text),
                    StyledSpan::text("42", theme.markdown.text),
                    StyledSpan::chrome(" ", theme.markdown.text),
                    StyledSpan::chrome("│", theme.border.subdued),
                ]),
                StyledLine::chrome("└───────┴───────┘", theme.border.subdued),
            ]
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.content.chars().any(|character| {
                    matches!(
                        character,
                        '┌' | '─' | '┬' | '┐' | '│' | '├' | '┼' | '┤' | '└' | '┴' | '┘'
                    )
                }))
                .all(|span| span.chrome)
        );
    }

    #[test]
    fn tables_measure_columns_and_honor_each_alignment() {
        let theme = Theme::system();
        let lines = render(
            "| Left | Center | Right | None |\n| :--- | :---: | ---: | --- |\n| a | b | c | d |",
            &theme,
        );

        assert_eq!(
            lines
                .iter()
                .map(StyledLine::written_text)
                .collect::<Vec<_>>(),
            [
                "┌──────┬────────┬───────┬──────┐",
                "│ Left │ Center │ Right │ None │",
                "├──────┼────────┼───────┼──────┤",
                "│ a    │   b    │     c │ d    │",
                "└──────┴────────┴───────┴──────┘",
            ]
        );
        for header in &lines[1].spans {
            if !header.chrome {
                assert!(header.style.add_modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn a_wide_table_wraps_its_widest_column_inside_continuous_borders() {
        let theme = Theme::system();
        let lines = painted(super::render(
            "| Name | Value |\n| --- | --- |\n| alpha beta gamma | delta |",
            &theme,
            22,
        ));

        assert_eq!(
            lines
                .iter()
                .map(StyledLine::written_text)
                .collect::<Vec<_>>(),
            [
                "┌────────────┬───────┐",
                "│ Name       │ Value │",
                "├────────────┼───────┤",
                "│ alpha beta │ delta │",
                "│ gamma      │       │",
                "└────────────┴───────┘",
            ]
        );
        assert!(lines.iter().all(|line| line.width() <= 22));
    }

    #[test]
    fn a_table_falls_back_to_pipe_text_only_below_its_column_floor() {
        let theme = Theme::system();
        let source = "| Name | Value |\n| --- | --- |\n| alpha beta gamma | delta |";

        let at_floor = painted(super::render(source, &theme, 13));
        assert!(at_floor[0].written_text().starts_with('┌'));
        assert!(at_floor.iter().all(|line| line.width() <= 13));

        let below_floor = painted(super::render(source, &theme, 12));
        assert_eq!(
            below_floor
                .iter()
                .map(StyledLine::written_text)
                .collect::<Vec<_>>(),
            [
                "| Name | Value | ",
                "| --- | --- |",
                "| alpha beta gamma | delta | ",
            ]
        );
        assert!(StyledLayout::new(&below_floor[2], 12).row_count() > 1);
    }

    #[test]
    fn table_measurement_uses_terminal_width_for_multibyte_cells() {
        let theme = Theme::system();
        let lines = render("| A | B |\n| --- | --- |\n| 界界 | x |", &theme);

        assert_eq!(
            lines
                .iter()
                .map(StyledLine::written_text)
                .collect::<Vec<_>>(),
            [
                "┌──────┬───┐",
                "│ A    │ B │",
                "├──────┼───┤",
                "│ 界界 │ x │",
                "└──────┴───┘",
            ]
        );
        assert!(lines.iter().all(|line| line.width() == 12));
    }

    #[test]
    fn table_cells_preserve_inline_styles_under_header_emphasis() {
        let theme = Theme::system();
        let lines = render(
            "| **Name** | Detail |\n| --- | --- |\n| **bold** | [link](https://example.test) |",
            &theme,
        );

        let span = |text| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == text)
                .unwrap_or_else(|| panic!("missing table span {text:?}"))
        };
        assert!(span("Name").style.add_modifier.contains(Modifier::BOLD));
        assert!(span("bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(
            span("link")
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(span(" (https://example.test)").chrome);
    }

    #[test]
    fn links_paint_the_label_and_destination_with_link_emphasis() {
        let theme = Theme::system();

        let lines = render("[Suru](https://example.test)", &theme);

        assert_eq!(
            lines,
            vec![StyledLine::from(vec![
                StyledSpan::text("Suru", theme.markdown.link_text),
                StyledSpan::chrome(" (https://example.test)", theme.markdown.link),
            ])]
        );
        assert!(!lines[0].spans[0].chrome);
        assert!(lines[0].spans[1].chrome);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn inline_autolink_and_reference_links_share_the_link_roles() {
        let mut theme = Theme::system();
        theme.markdown.link_text = Style::default().fg(Color::Indexed(121));
        theme.markdown.link = Style::default().fg(Color::Indexed(122));
        let lines = render(
            "[inline](https://inline.test) <https://auto.test> [reference][target]\n\n[target]: https://reference.test",
            &theme,
        );

        for label in ["inline", "https://auto.test", "reference"] {
            let span = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == label)
                .unwrap_or_else(|| panic!("missing link label {label:?}"));
            assert_eq!(span.style, theme.markdown.link_text, "{label}");
            assert!(!span.chrome, "{label}");
        }
        for destination in [
            " (https://inline.test)",
            " (https://auto.test)",
            " (https://reference.test)",
        ] {
            let span = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == destination)
                .unwrap_or_else(|| panic!("missing link destination {destination:?}"));
            assert_eq!(span.style, theme.markdown.link, "{destination}");
            assert!(span.chrome, "{destination}");
        }
    }

    #[test]
    fn images_paint_an_explicit_chrome_wrapper_with_distinct_roles() {
        let mut theme = Theme::system();
        theme.markdown.image_text = Style::default().fg(Color::Indexed(131));
        theme.markdown.image = Style::default().fg(Color::Indexed(132));

        let lines = render("![alt text](https://image.test)", &theme);

        assert_eq!(
            lines,
            vec![StyledLine::from(vec![
                StyledSpan::chrome("[image: ", theme.markdown.image),
                StyledSpan::text("alt text", theme.markdown.image_text),
                StyledSpan::chrome("]", theme.markdown.image),
                StyledSpan::chrome(" (https://image.test)", theme.markdown.image),
            ])]
        );
    }

    #[test]
    fn image_chrome_and_text_inherit_outer_inline_modifiers() {
        let theme = Theme::system();
        let lines = render("**![alt](https://image.test)**", &theme);

        assert_eq!(
            lines[0]
                .spans
                .iter()
                .map(|span| (span.content.as_str(), span.chrome))
                .collect::<Vec<_>>(),
            [
                ("[image: ", true),
                ("alt", false),
                ("]", true),
                (" (https://image.test)", true),
            ]
        );
        assert!(lines[0].spans.iter().all(|span| {
            span.style.add_modifier.contains(Modifier::BOLD)
                && span.style.fg
                    == if span.content == "alt" {
                        theme.markdown.image_text.fg
                    } else {
                        theme.markdown.image.fg
                    }
        }));
    }

    #[test]
    fn an_image_with_empty_alt_text_keeps_a_complete_visual_placeholder() {
        let theme = Theme::system();
        let lines = render("![](https://image.test)", &theme);

        assert_eq!(line_texts(&lines), ["[image: ] (https://image.test)"]);
        assert!(lines[0].spans.iter().all(|span| span.chrome));
    }

    #[test]
    fn optional_markdown_roles_paint_their_semantic_surfaces() {
        let mut theme = Theme::system();
        theme.markdown.text = Style::default().fg(Color::Indexed(101));
        theme.markdown.link_text = Style::default().fg(Color::Indexed(102));
        theme.markdown.link = Style::default().fg(Color::Indexed(103));
        theme.markdown.image_text = Style::default().fg(Color::Indexed(104));
        theme.markdown.image = Style::default().fg(Color::Indexed(105));
        theme.markdown.block_quote = Style::default().fg(Color::Indexed(106));
        theme.markdown.list_marker = Style::default().fg(Color::Indexed(107));
        theme.markdown.list_enumeration = Style::default().fg(Color::Indexed(108));
        theme.markdown.rule = Style::default().fg(Color::Indexed(109));

        let markdown = "body [label](https://link.test) ![alt](https://image.test)\n\n> quote\n\n- bullet\n\n1. ordinal\n\n---";
        let lines = render(markdown, &theme);
        let span = |content| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == content)
                .unwrap_or_else(|| panic!("missing span {content:?}"))
        };

        assert_eq!(span("body ").style, theme.markdown.text);
        assert_eq!(span("label").style, theme.markdown.link_text);
        assert_eq!(span(" (https://link.test)").style, theme.markdown.link);
        assert_eq!(span("alt").style, theme.markdown.image_text);
        assert_eq!(span(" (https://image.test)").style, theme.markdown.image);
        assert_eq!(span("│ ").style, theme.markdown.block_quote);
        assert_eq!(span("• ").style, theme.markdown.list_marker);
        assert_eq!(span("1. ").style, theme.markdown.list_enumeration);
        let rule = "─".repeat(80);
        assert_eq!(span(&rule).style, theme.markdown.rule);

        let reasoning = render_reasoning(markdown, &theme);
        assert!(
            reasoning
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg == theme.text.subdued.fg),
            "every Markdown role follows Reasoning's subdued posture"
        );
    }

    #[test]
    fn horizontal_rules_fill_each_available_width_as_rule_chrome() {
        let theme = Theme::system();

        for width in [9, 23] {
            let lines = super::render("---", &theme, width);

            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].width(), usize::from(width));
            assert_eq!(lines[0].spans[0].content, "─".repeat(usize::from(width)));
            assert_eq!(lines[0].spans[0].style, theme.markdown.rule);
            assert!(lines[0].spans[0].chrome);
            assert!(lines[0].spans[0].source.is_some());
        }
    }

    #[test]
    fn horizontal_rules_reserve_nested_quote_and_list_prefixes() {
        let theme = Theme::system();
        let lines = super::render("> - before\n>\n>   ---", &theme, 17);
        let rule = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains('─')))
            .expect("nested rule is painted");

        assert_eq!(rule.width(), 17);
        assert_eq!(rule.spans[0].content, "│ ");
        assert_eq!(rule.spans[1].content, "  ");
        assert_eq!(rule.spans[2].content, "─".repeat(13));
    }

    #[test]
    fn horizontal_rules_handle_zero_and_one_cell_content_widths() {
        let theme = Theme::system();

        assert!(super::render("---", &theme, 0).is_empty());
        let one_cell = super::render("---", &theme, 1);
        assert_eq!(line_texts(&one_cell), ["─"]);
        assert!(one_cell[0].spans[0].chrome);
    }

    #[test]
    fn reasoning_rules_fill_width_in_the_subdued_style() {
        let theme = Theme::system();
        let lines = super::render_reasoning("---", &theme, 19);

        assert_eq!(lines[0].width(), 19);
        assert_eq!(lines[0].spans[0].style, theme.text.subdued);
        assert!(lines[0].spans[0].chrome);
    }

    #[test]
    fn inline_emphasis_paints_italic_and_bold_modifiers() {
        let theme = Theme::system();
        let italic = theme.markdown.text.patch(theme.markdown.emphasis);
        let bold = theme.markdown.text.patch(theme.markdown.strong);

        let lines = render("plain *soft* and **strong**", &theme);

        assert_eq!(
            lines,
            vec![StyledLine::from(vec![
                StyledSpan::text("plain ", theme.markdown.text),
                StyledSpan::text("soft", italic),
                StyledSpan::text(" and ", theme.markdown.text),
                StyledSpan::text("strong", bold),
            ])]
        );
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(lines[0].spans.iter().all(|span| !span.chrome));
    }

    #[test]
    fn strikethrough_patches_crossed_out_onto_nested_inline_styles() {
        let theme = Theme::system();

        let lines = render(
            "plain ~~gone **bold** `code` [link](https://example.test)~~",
            &theme,
        );
        let gone = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "gone ")
            .expect("struck prose is painted without its delimiters");
        let bold = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "bold")
            .expect("nested strong prose remains its own span");
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "code")
            .expect("nested inline code remains its own span");
        let link = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "link")
            .expect("nested link label remains its own span");
        let destination = lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("https://example.test"))
            .expect("nested link destination remains visible chrome");

        assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(bold.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(code.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(code.style.fg, theme.markdown.inline_code.fg);
        assert!(link.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(link.style.fg, theme.markdown.link_text.fg);
        assert!(
            destination
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn markdown_structure_sets_the_continuation_indent_when_wrapped() {
        let theme = Theme::system();
        let cases = [
            (
                "- first item that wraps onto another row",
                0,
                vec![
                    (0, "• first item that".to_owned()),
                    (2, "  wraps onto another".to_owned()),
                    (2, "  row".to_owned()),
                ],
            ),
            (
                "> quoted words that wrap onto another row",
                0,
                vec![
                    (0, "│ quoted words that".to_owned()),
                    (2, "  wrap onto another".to_owned()),
                    (2, "  row".to_owned()),
                ],
            ),
            (
                "> > nested quoted words that wrap onto another row",
                0,
                vec![
                    (0, "│ │ nested quoted".to_owned()),
                    (4, "    words that wrap".to_owned()),
                    (4, "    onto another row".to_owned()),
                ],
            ),
        ];

        for (markdown, line_index, expected) in cases {
            let lines = super::render(markdown, &theme, 20);
            let rows = StyledLayout::new(&lines[line_index], 20)
                .rows()
                .iter()
                .map(|row| {
                    (
                        row.indent,
                        row.line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                            .trim_end()
                            .to_owned(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(rows, expected, "wrap changed for {markdown:?}");
        }
    }

    #[test]
    fn harmless_inline_html_projects_to_markdown_paint() {
        let theme = Theme::system();
        let lines = render(
            "one<br>two <b>bold</b> <i>soft</i> <code>code</code> <kbd>key</kbd> <a href='https://example.test/?q=R&D=*stars*;&amp;b=2'>site</a> <img alt=\"map &amp; key\" src='map.png'> <code><b>still</b></code>",
            &theme,
        );

        assert_eq!(
            line_texts(&lines),
            [
                "one",
                "two bold soft code key site (https://example.test/?q=R&D=*stars*;&b=2) [image: map & key] (map.png) still"
            ]
        );
        let spans = lines
            .iter()
            .flat_map(|line| &line.spans)
            .collect::<Vec<_>>();
        assert!(
            spans
                .iter()
                .find(|span| span.content == "bold")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            spans
                .iter()
                .find(|span| span.content == "soft")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        for content in ["code", "key", "still"] {
            assert_eq!(
                spans
                    .iter()
                    .find(|span| span.content == content)
                    .unwrap()
                    .style
                    .fg,
                theme.markdown.inline_code.fg
            );
        }
    }

    #[test]
    fn html_blocks_keep_inner_text_and_malformed_tags_do_not_unbalance_paint() {
        let theme = Theme::system();
        let lines = render(
            "<details><summary>x &amp; y &copy;</summary>body R&D with *stars*; and `ticks`;</details>\n\n<b>bold\n\nplain <b open",
            &theme,
        );

        assert_eq!(
            line_texts(&lines),
            [
                "x & y ©body R&D with *stars*; and `ticks`;",
                "",
                "bold",
                "",
                "plain <b open"
            ]
        );
        assert!(
            lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines.last().unwrap().spans[0].style, theme.markdown.text);
    }

    #[test]
    fn html_blocks_preserve_comparison_text_that_only_looks_like_a_tag() {
        let theme = Theme::system();
        let lines = super::render(
            "<details open class='example'>a < b and c > d; a < 2 then <b>bold</b></details>",
            &theme,
            80,
        );

        assert_eq!(line_texts(&lines), ["a < b and c > d; a < 2 then bold"]);
        let spans = lines
            .iter()
            .flat_map(|line| &line.spans)
            .collect::<Vec<_>>();
        assert_eq!(spans[0].style, theme.markdown.text);
        assert!(
            spans
                .iter()
                .find(|span| span.content == "bold")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );

        let document = lines[0].markdown.clone().unwrap();
        let ranges = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.source.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            document.copy(&ranges, true),
            crate::tui::ClipboardContent {
                text: r"a \< b and c \> d; a \< 2 then **bold**".into(),
                html: Some(
                    "<p>a &lt; b and c &gt; d; a &lt; 2 then <strong>bold</strong></p>\n".into(),
                ),
            }
        );
    }

    #[test]
    fn html_code_and_kbd_keep_footnote_syntax_literal_and_atomic() {
        let theme = Theme::system();
        let source = "See <code>x[^n]y</code> and <kbd>k[^n]z</kbd>.\n\n[^n]: note";
        let lines = super::render(source, &theme, 80);

        assert_eq!(line_texts(&lines)[0], "See x[^n]y and k[^n]z.");
        for content in ["x[^n]y", "k[^n]z"] {
            let span = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == content)
                .unwrap();
            assert_eq!(span.style.fg, theme.markdown.inline_code.fg);
        }

        let document = lines[0].markdown.clone().unwrap();
        let ranges = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.source.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            document.copy(&ranges, true),
            crate::tui::ClipboardContent {
                text: "See `x[^n]y` and `k[^n]z`.\n\n[^n]: note".into(),
                html: Some(
                    "<p>See <code>x[^n]y</code> and <code>k[^n]z</code>.</p>\n<div class=\"footnote-definition\" id=\"n\"><sup class=\"footnote-definition-label\">1</sup>\n<p>note</p>\n</div>\n"
                        .into(),
                ),
            }
        );
    }

    #[test]
    fn footnotes_use_authored_numbers_and_move_definitions_after_a_rule() {
        let theme = Theme::system();
        let markdown = "Body[^later] then[^first].\n\n[^first]: first\n\n[^later]: later\n\n    9) nested\n\n1) main";
        let lines = render(markdown, &theme);
        let text = line_texts(&lines);

        assert_eq!(text[0], "Body[2] then[1].");
        let rule = text
            .iter()
            .position(|line| !line.is_empty() && line.chars().all(|c| c == '─'))
            .unwrap();
        assert!(text[rule + 1].starts_with("  [1] first"), "{text:?}");
        assert!(text.iter().any(|line| line.contains("9) nested")));
        assert_eq!(text[2], "1) main");
        let reference = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "[2]")
            .unwrap();
        assert_eq!(reference.style, theme.text.subdued);

        let reasoning = render_reasoning(markdown, &theme);
        assert!(
            reasoning
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg == theme.text.subdued.fg)
        );
    }

    #[test]
    fn html_and_footnotes_copy_as_safe_markdown_structure() {
        let lines = super::render(
            "[^note]: foot <kbd>key</kbd>\n\nSee <strong>bold</strong> <a href='https://example.test/?x=1&amp;y=2'>there</a>[^note]. <img alt='plot' src='https://example.test/p.png'>",
            &Theme::system(),
            80,
        );
        let document = lines[0].markdown.clone().unwrap();
        let ranges = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.source.clone())
            .collect::<Vec<_>>();
        let copied = document.copy(&ranges, true);

        assert_eq!(
            copied.text,
            "See **bold** [there](https://example.test/?x=1&amp;y=2)[^note]. ![plot](https://example.test/p.png)\n\n[^note]: foot `key`"
        );
        let html = copied.html.unwrap();
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("href=\"https://example.test/?x=1&amp;y=2\""));
        assert!(html.contains("<code>key</code>"));
        assert!(html.contains("href=\"https://example.test/p.png\""));
    }

    #[test]
    fn disabled_markdown_extensions_stay_literal_while_html_super_subscripts_keep_text() {
        let lines = render(
            "$x^2$ and ^raised^, <sup>high</sup>/<sub>low</sub>",
            &Theme::system(),
        );

        assert_eq!(line_texts(&lines), ["$x^2$ and ^raised^, high/low"]);
    }

    #[test]
    fn selected_rule_round_trips_as_markdown() {
        let lines = super::render("---", &Theme::system(), 80);
        let document = lines[0]
            .markdown
            .clone()
            .expect("rendered Markdown carries its copy document");
        let ranges = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.source.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            document.copy(&ranges, true),
            crate::tui::ClipboardContent {
                text: "---".into(),
                html: Some("<hr />\n".into()),
            }
        );
    }

    #[test]
    fn streaming_fences_keep_their_colors_when_closed() {
        let theme = Theme::system();
        for fence in ["```", "~~~"] {
            let mut message = format!("Example:\n\n{fence}rust\n");
            for delta in ["let", " name = ", "\"hello\";", "\n// note", "\n"] {
                message.push_str(delta);
                let answer = render(&message, &theme);
                assert!(answer.iter().flat_map(|line| &line.spans).any(|span| {
                    span.content == "let"
                        && span.style == theme.syntax.keyword.add_modifier(Modifier::ITALIC)
                }));
                let reasoning = render_reasoning(&message, &theme);
                let code_style = theme
                    .text
                    .subdued
                    .add_modifier(theme.markdown.code_block.add_modifier);
                assert!(
                    reasoning
                        .iter()
                        .flat_map(|line| &line.spans)
                        .any(|span| { span.content.contains("let") && span.style == code_style })
                );
                assert!(!reasoning.iter().flat_map(|line| &line.spans).any(|span| {
                    span.style == theme.syntax.keyword.add_modifier(Modifier::ITALIC)
                }));
                assert_eq!(
                    reasoning
                        .iter()
                        .flat_map(|line| &line.spans)
                        .find(|span| span.content == "rust")
                        .unwrap()
                        .style,
                    theme.text.subdued
                );

                let separator = if message.ends_with('\n') { "" } else { "\n" };
                let closed = format!("{message}{separator}{fence}");
                assert_eq!(answer, render(&closed, &theme));
                assert_eq!(reasoning, render_reasoning(&closed, &theme));
            }
        }
    }

    #[test]
    fn oversized_code_blocks_fall_back_at_the_byte_boundary() {
        let theme = Theme::system();
        for bytes in [32_767, 32_768, 32_769] {
            // Multibyte padding distinguishes a byte limit from a character limit.
            let mut content = "let n = 42;\n//".to_owned();
            content.push_str(&"é".repeat((bytes - content.len() - 1) / 2));
            content.push_str(&" ".repeat(bytes - content.len() - 1));
            content.push('\n');
            let lines = render(&format!("```rust\n{content}```"), &theme);
            if bytes > 32_768 {
                let mut flat = render(&format!("```unknown\n{content}```"), &theme);
                flat[0] = StyledLine::chrome("rust", theme.text.subdued);
                assert_eq!(lines, flat);
            } else {
                assert!(
                    lines[1].spans.iter().any(|span| {
                        span.content == "let"
                            && span.style == theme.syntax.keyword.add_modifier(Modifier::ITALIC)
                    }),
                    "{bytes} bytes should be highlighted"
                );
            }
        }
    }

    #[test]
    fn reasoning_code_blocks_keep_the_same_markdown_copy_source() {
        let source = "Example\n\n```rust\nlet answer = 42;\n```";
        for render in [super::render, super::render_reasoning] {
            let lines = render(source, &Theme::system(), 80);
            let document = lines[0].markdown.clone().unwrap();
            let ranges = lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| span.source.clone())
                .collect::<Vec<_>>();

            assert_eq!(document.copy(&ranges, true).text, source);
        }
    }

    #[test]
    fn fence_snippets_use_their_grammar() {
        let theme = Theme::system();
        for (language, content, token, style) in [
            (
                "typescript",
                "interface Widget { name: string; }",
                "Widget",
                theme.syntax.r#type.add_modifier(Modifier::BOLD),
            ),
            (
                "python",
                "def greet():\n    return \"hello\"",
                "return",
                theme.syntax.keyword.add_modifier(Modifier::ITALIC),
            ),
            (
                "sh",
                "if true; then echo \"hello\"; fi",
                "if",
                theme.syntax.keyword.add_modifier(Modifier::ITALIC),
            ),
            (
                "toml",
                "[package]\nname = \"hello\"",
                "hello",
                theme.syntax.string,
            ),
            (
                "json",
                "{\"name\": \"hello\", \"count\": 42}",
                "42",
                theme.syntax.number,
            ),
        ] {
            let lines = render(&format!("```{language}\n{content}\n```"), &theme);
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| span.content.contains(token) && span.style == style),
                "{language}: {lines:?}"
            );
        }
    }

    #[test]
    fn unnamed_and_unknown_fences_keep_flat_output_and_inline_code_is_unchanged() {
        let theme = Theme::system();
        for info in ["", "not-a-language"] {
            for (render, style) in [
                (
                    render as fn(&str, &Theme) -> Vec<StyledLine>,
                    theme.markdown.code_block,
                ),
                (render_reasoning, theme.text.subdued),
            ] {
                let mut expected = Vec::new();
                if !info.is_empty() {
                    expected.push(StyledLine::chrome(info, theme.text.subdued));
                }
                expected.extend([
                    StyledLine::text("let n = 42;", style),
                    StyledLine::default(),
                    StyledLine::text("  # still literal", style),
                ]);
                assert_eq!(
                    render(
                        &format!("```{info}\nlet n = 42;\n\n  # still literal\n```"),
                        &theme
                    ),
                    expected
                );
            }
        }
        assert_eq!(
            render("`let n = 42;`", &theme),
            vec![StyledLine::text("let n = 42;", theme.markdown.inline_code)]
        );
    }

    #[test]
    fn multiline_grammar_state_and_unicode_content_survive_projection() {
        let theme = Theme::system();
        let lines = render(
            "```rust\n/* start\nstill 注釈 */\nlet café = \"☕\";\n```",
            &theme,
        );
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|span| span.content.contains("still 注釈")
                    && span.style == theme.syntax.comment.add_modifier(Modifier::ITALIC))
        );
        assert_eq!(
            lines[3]
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>(),
            "let café = \"☕\";"
        );
    }

    #[test]
    fn jsx_fences_paint_tags_attributes_and_expressions() {
        let theme = Theme::system();
        for info in ["jsx", "javascriptreact"] {
            let lines = render(
                &format!("```{info}\nconst view = <div title=\"hello\">{{1 + 2}}</div>;\n```"),
                &theme,
            );
            for (token, style) in [
                ("div", theme.feedback.error),
                ("title", theme.feedback.warning),
                ("hello", theme.syntax.string),
                ("1", theme.syntax.number),
            ] {
                assert!(
                    lines
                        .iter()
                        .flat_map(|line| &line.spans)
                        .any(|span| { span.content.contains(token) && span.style == style }),
                    "missing {token:?} highlighting for {info:?}: {lines:?}"
                );
            }
        }
    }

    #[test]
    fn csharp_fence_paints_keywords() {
        let theme = Theme::system();
        for info in ["csharp", "CSharp title=demo", "cs", "c#"] {
            let lines = render(&format!("```{info}\npublic class Widget {{}}\n```"), &theme);
            assert!(
                lines.iter().flat_map(|line| &line.spans).any(|span| {
                    span.content.contains("public")
                        && span.style == theme.syntax.keyword.add_modifier(Modifier::ITALIC)
                }),
                "C# keyword should be highlighted for {info:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn rust_fence_paints_keywords_strings_comments_and_types() {
        let theme = Theme::system();
        let lines = render(
            "```rust\nstruct Widget;\nlet name = \"hello\"; // note\n```",
            &theme,
        );
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        for (text, style) in [
            (
                "struct",
                theme.syntax.keyword.add_modifier(Modifier::ITALIC),
            ),
            ("Widget", theme.syntax.r#type.add_modifier(Modifier::BOLD)),
            ("hello", theme.syntax.string),
            ("//", theme.syntax.comment.add_modifier(Modifier::ITALIC)),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.content.contains(text) && span.style == style),
                "missing {text:?} with {style:?}: {spans:?}"
            );
        }
        assert_eq!(lines[0], StyledLine::chrome("rust", theme.text.subdued));
    }

    #[test]
    fn supported_hyperlinks_hide_destination_chrome_and_keep_a_safe_target() {
        let theme = Theme::system();
        let supported = super::render_with_hyperlinks(
            "Read [the guide]( https://example.test/guide ).",
            &theme,
            80,
            true,
        );
        let fallback = super::render_with_hyperlinks(
            "Read [the guide]( https://example.test/guide ).",
            &theme,
            80,
            false,
        );

        assert_eq!(line_texts(&supported), ["Read the guide."]);
        assert_eq!(
            line_texts(&fallback),
            ["Read the guide (https://example.test/guide)."]
        );
        let target = supported[0]
            .spans
            .iter()
            .find(|span| span.content == "the guide")
            .and_then(|span| span.target.as_deref());
        assert_eq!(target, Some("https://example.test/guide"));
    }

    #[test]
    fn unsafe_decoded_destination_stays_visible_without_a_target() {
        let theme = Theme::system();
        let lines = super::render_with_hyperlinks(
            "[label](https://example.test/a&#27;bad)",
            &theme,
            80,
            true,
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.target.is_none())
        );
        assert!(line_texts(&lines)[0].contains("https://example.test/abad"));
    }

    #[test]
    fn reasoning_links_use_the_same_capability_without_losing_subdued_style() {
        let theme = Theme::system();
        let lines = super::render_reasoning_with_hyperlinks(
            "[guide](https://example.test/path)",
            &theme,
            80,
            true,
        );
        assert_eq!(line_texts(&lines), ["guide"]);
        let span = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "guide")
            .unwrap();
        assert_eq!(span.target.as_deref(), Some("https://example.test/path"));
        assert_eq!(span.style.fg, theme.text.subdued.fg);
    }

    #[test]
    fn relative_links_keep_visible_chrome_without_an_invented_process_base() {
        let theme = Theme::system();
        let lines = super::render_with_hyperlinks("[guide](guide.md)", &theme, 80, true);
        assert_eq!(line_texts(&lines), ["guide (guide.md)"]);
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.target.is_none())
        );
    }

    #[test]
    fn a_linked_image_uses_the_outer_anchor_as_its_actionable_target() {
        let theme = Theme::system();
        let lines = super::render_with_hyperlinks(
            "[![alt](image.png)](https://example.test/page)",
            &theme,
            80,
            true,
        );
        assert_eq!(line_texts(&lines), ["[image: alt] (image.png)"]);
        assert!(
            lines[0]
                .spans
                .iter()
                .filter(|span| span.content.contains("image:") || span.content == "alt")
                .all(|span| span.target.as_deref() == Some("https://example.test/page"))
        );
    }
}
