//! Practical Markdown projection for Agent-authored transcript content.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::theme::Theme;

mod syntax;

pub(super) fn render(content: &str, theme: &Theme) -> Vec<Line<'static>> {
    render_prose(content, theme, false)
}

pub(super) fn render_reasoning(content: &str, theme: &Theme) -> Vec<Line<'static>> {
    render_prose(content, theme, true)
}

fn render_prose(content: &str, theme: &Theme, subdued: bool) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(content, Options::empty());
    let mut renderer = Renderer::new(theme, subdued);
    for event in parser {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer<'a> {
    theme: &'a Theme,
    subdued: bool,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    links: Vec<String>,
    code_block: Option<CodeBlock>,
}

struct CodeBlock {
    info: String,
    content: String,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme, subdued: bool) -> Self {
        Self {
            theme,
            subdued,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![theme.text.primary],
            lists: Vec::new(),
            links: Vec::new(),
            code_block: None,
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
                if let Some(block) = &mut self.code_block {
                    block.content.push('\n');
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
                self.code_block = Some(CodeBlock {
                    info: match &kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    },
                    content: String::new(),
                });
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
                if let Some(block) = self.code_block.take() {
                    self.lines.extend(syntax::render(
                        &block.content,
                        &block.info,
                        self.theme,
                        self.prose_style(self.theme.markdown.code_block),
                    ));
                }
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
        if let Some(block) = &mut self.code_block {
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

    fn push(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            self.current
                .push(Span::styled(content, self.prose_style(style)));
        }
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

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
                    render as fn(&str, &Theme) -> Vec<Line<'static>>,
                    theme.markdown.code_block,
                ),
                (render_reasoning, theme.text.subdued),
            ] {
                let mut expected = Vec::new();
                if !info.is_empty() {
                    expected.push(Line::from(Span::styled(info, theme.text.subdued)));
                }
                expected.extend([
                    Line::from(Span::styled("let n = 42;", style)),
                    Line::default(),
                    Line::from(Span::styled("  # still literal", style)),
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
            vec![Line::from(Span::styled(
                "let n = 42;",
                theme.markdown.inline_code
            ))]
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
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "let café = \"☕\";"
        );
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
        assert_eq!(
            lines[0],
            Line::from(Span::styled("rust", theme.text.subdued))
        );
    }
}
