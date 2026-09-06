//! Practical Markdown projection for Agent-authored transcript content.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;

use crate::theme::Theme;

use super::text_layout::{StyledLine, StyledSpan};

mod syntax;

pub(super) fn render(content: &str, theme: &Theme) -> Vec<StyledLine> {
    render_prose(content, theme, false)
}

pub(super) fn render_reasoning(content: &str, theme: &Theme) -> Vec<StyledLine> {
    render_prose(content, theme, true)
}

fn render_prose(content: &str, theme: &Theme, subdued: bool) -> Vec<StyledLine> {
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
    lines: Vec<StyledLine>,
    current: Vec<StyledSpan>,
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
                    // The label names the fence rather than belonging to the
                    // code, so a copy of the Code Block leaves it out.
                    self.push_chrome(language.as_ref(), self.theme.text.subdued);
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
                    self.lines.extend(
                        syntax::render(
                            &block.content,
                            &block.info,
                            self.theme,
                            self.prose_style(self.theme.markdown.code_block),
                        )
                        .into_iter()
                        .map(StyledLine::from),
                    );
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
                .push(StyledSpan::text(content, self.prose_style(style)));
        }
    }

    fn push_chrome(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            self.current
                .push(StyledSpan::chrome(content, self.prose_style(style)));
        }
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            self.lines
                .push(StyledLine::from(std::mem::take(&mut self.current)));
        }
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if !self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.push(StyledLine::default());
        }
    }

    fn finish(mut self) -> Vec<StyledLine> {
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
    fn streaming_fences_keep_their_colors_when_closed() {
        let theme = Theme::system();
        for render in [render, render_reasoning] {
            for fence in ["```", "~~~"] {
                let mut message = format!("Example:\n\n{fence}rust\n");
                for delta in ["let", " name = ", "\"hello\";", "\n// note", "\n"] {
                    message.push_str(delta);
                    let partial = render(&message, &theme);
                    assert!(partial.iter().flat_map(|line| &line.spans).any(|span| {
                        span.content == "let"
                            && span.style == theme.syntax.keyword.add_modifier(Modifier::ITALIC)
                    }));
                    let separator = if message.ends_with('\n') { "" } else { "\n" };
                    assert_eq!(
                        partial,
                        render(&format!("{message}{separator}{fence}"), &theme)
                    );
                }
            }
        }
    }

    #[test]
    fn oversized_code_blocks_fall_back_at_the_byte_boundary() {
        let theme = Theme::system();
        for render in [render, render_reasoning] {
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
}
