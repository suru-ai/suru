//! Practical Markdown projection for Agent-authored transcript content.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::theme::Theme;

pub(super) fn render(content: &str, theme: &Theme) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(content, Options::empty());
    let mut renderer = Renderer::new(theme);
    for event in parser {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer<'a> {
    theme: &'a Theme,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    links: Vec<String>,
    in_code_block: bool,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![theme.text.primary],
            lists: Vec::new(),
            links: Vec::new(),
            in_code_block: false,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(text.as_ref()),
            Event::Code(code) => self.push(code.as_ref(), self.theme.markdown.inline_code),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.push(math.as_ref(), self.current_style())
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.text(html.as_ref());
            }
            Event::FootnoteReference(label) => {
                self.push(format!("[{label}]"), self.theme.text.subdued);
            }
            Event::SoftBreak => {
                if self.in_code_block {
                    self.flush_code_line();
                } else {
                    self.push(" ", self.current_style());
                }
            }
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.push("────────", self.theme.border.subdued);
                self.flush_line();
                self.blank_line();
            }
            Event::TaskListMarker(checked) => self.push(
                if checked { "[x] " } else { "[ ] " },
                self.theme.markdown.list_marker,
            ),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => self.push_style(self.theme.markdown.heading),
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    self.push(language.as_ref(), self.theme.text.subdued);
                    self.flush_line();
                }
            }
            Tag::List(first) => self.lists.push(first),
            Tag::Item => {
                self.flush_line();
                let depth = self.lists.len().saturating_sub(1);
                if depth > 0 {
                    self.push("  ".repeat(depth), self.theme.text.primary);
                }
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}. ");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.push(marker, self.theme.markdown.list_marker);
            }
            Tag::Emphasis => self.push_style(self.theme.markdown.emphasis),
            Tag::Strong => self.push_style(self.theme.markdown.strong),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.push_style(self.theme.markdown.link);
            }
            Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.push_style(self.theme.markdown.link);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.push("│ ", self.theme.markdown.list_marker);
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Strikethrough
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
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line();
                self.blank_line();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis | TagEnd::Strong => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.pop_style();
                if let Some(destination) = self.links.pop() {
                    self.push(format!(" ({destination})"), self.theme.markdown.link);
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blank_line();
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn text(&mut self, text: &str) {
        if !self.in_code_block {
            self.push(text, self.current_style());
            return;
        }

        let mut remaining = text;
        while let Some((line, rest)) = remaining.split_once('\n') {
            self.push(line, self.theme.markdown.code_block);
            self.flush_code_line();
            remaining = rest;
        }
        if !remaining.is_empty() {
            self.push(remaining, self.theme.markdown.code_block);
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

    fn current_style(&self) -> Style {
        *self
            .styles
            .last()
            .expect("Markdown style stack is non-empty")
    }

    fn push(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            self.current.push(Span::styled(content, style));
        }
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
    }

    fn flush_code_line(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if !self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}
