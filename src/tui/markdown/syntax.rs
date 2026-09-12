//! Fence-selected grammars and the translation from TextMate scopes to Theme roles.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::LazyLock,
};

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

use crate::theme::Theme;

// Bound per-delta parsing work; oversized blocks retain the flat code style.
const MAX_HIGHLIGHT_BYTES: usize = 32 * 1024;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

fn language<'a>(info: &str, syntaxes: &'a SyntaxSet) -> Option<&'a SyntaxReference> {
    let token = info.split_whitespace().next()?.to_ascii_lowercase();
    let token = match token.as_str() {
        "js" | "javascript" => "js",
        "ts" | "typescript" => "ts",
        // The fancy-regex bundle omits Babel; TSX also understands JSX.
        "jsx" | "javascriptreact" => "tsx",
        "sh" | "shell" | "bash" | "zsh" | "shellscript" => "sh",
        "py" | "python" => "py",
        "rs" | "rust" => "rs",
        "csharp" => "cs",
        // `fs` is ambiguous and resolves to GLSL in the bundled syntax set.
        "fsharp" => "F#",
        "golang" => "go",
        "objc" | "objectivec" | "obj-c" => "Objective-C",
        "cplusplus" => "C++",
        "batch" | "dos" => "bat",
        "yml" | "yaml" => "yaml",
        "docker" | "dockerfile" => "Dockerfile",
        other => other,
    };
    syntaxes.find_syntax_by_token(token)
}

pub(super) fn render(
    content: &str,
    info: &str,
    theme: &Theme,
    flat_style: Style,
) -> Vec<Line<'static>> {
    if content.len() > MAX_HIGHLIGHT_BYTES {
        return flat_lines(content, flat_style);
    }
    render_with_syntaxes(content, info, theme, &SYNTAXES, flat_style)
}

fn render_with_syntaxes(
    content: &str,
    info: &str,
    theme: &Theme,
    syntaxes: &SyntaxSet,
    flat_style: Style,
) -> Vec<Line<'static>> {
    language(info, syntaxes)
        .and_then(|syntax| {
            // Syntect returns parse errors, but malformed grammar regexes can
            // also panic. Neither may take down transcript rendering.
            catch_unwind(AssertUnwindSafe(|| {
                highlight(content, syntax, syntaxes, theme)
            }))
            .ok()
            .and_then(Result::ok)
        })
        .unwrap_or_else(|| flat_lines(content, flat_style))
}

pub(super) fn flat_lines(content: &str, style: Style) -> Vec<Line<'static>> {
    content
        .split_terminator('\n')
        .map(|line| {
            if line.is_empty() {
                Line::default()
            } else {
                Line::from(Span::styled(line.to_owned(), style))
            }
        })
        .collect()
}

fn highlight(
    content: &str,
    syntax: &SyntaxReference,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) -> Result<Vec<Line<'static>>, syntect::Error> {
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(content) {
        let operations = parser.parse_line(line, syntaxes)?;
        let visible_end = line.strip_suffix('\n').unwrap_or(line).len();
        let mut spans = Vec::new();
        let mut start = 0;
        for (offset, operation) in operations {
            let end = offset.min(visible_end);
            if start < end {
                spans.push(Span::styled(
                    line[start..end].to_owned(),
                    stack_style(&stack, theme),
                ));
            }
            stack.apply(&operation)?;
            start = end;
        }
        if start < visible_end {
            spans.push(Span::styled(
                line[start..visible_end].to_owned(),
                stack_style(&stack, theme),
            ));
        }
        lines.push(Line::from(spans));
    }
    Ok(lines)
}

fn stack_style(stack: &ScopeStack, theme: &Theme) -> Style {
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| scope_style(&scope.to_string(), theme))
        .unwrap_or(theme.markdown.code_block)
}

/// TextMate's dotted scopes translated to OpenCode's syntax roles. More specific
/// rules precede their families; an unrecognized scope lets its parent supply style.
fn scope_style(scope: &str, theme: &Theme) -> Option<Style> {
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|s| s.starts_with('.'))
    };
    let style = if is("comment") || is("punctuation.definition.comment") {
        theme.syntax.comment.add_modifier(Modifier::ITALIC)
    } else if is("entity.other.attribute-name")
        || is("meta.annotation")
        || is("storage.type.annotation")
        || is("entity.name.function.decorator")
    {
        theme.feedback.warning
    } else if is("support.function")
        || is("support.type")
        || is("support.class")
        || is("support.constant")
        || is("variable.language")
        || is("entity.name.tag")
    {
        theme.feedback.error
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
    {
        theme.syntax.r#type.add_modifier(Modifier::BOLD)
    } else if is("keyword.operator") {
        theme.syntax.operator
    } else if is("keyword") || is("storage") {
        theme.syntax.keyword.add_modifier(Modifier::ITALIC)
    } else if is("constant.character.escape") || is("string.regexp") {
        theme.syntax.keyword
    } else if is("string") || is("constant.character") || is("punctuation.definition.string") {
        theme.syntax.string
    } else if is("constant") {
        theme.syntax.number
    } else if is("entity.name.function") || is("entity.name.method") {
        theme.syntax.function
    } else if is("entity.name.namespace") || is("entity.name.module") {
        theme.syntax.r#type
    } else if is("variable") {
        theme.syntax.variable
    } else if is("punctuation.separator")
        || is("punctuation.accessor")
        || is("punctuation.definition.tag")
    {
        theme.syntax.operator
    } else if is("punctuation") {
        theme.syntax.punctuation
    } else {
        return None;
    };
    Some(theme.markdown.code_block.patch(style))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighter_failure_restores_the_entire_flat_block() {
        use syntect::parsing::SyntaxDefinition;
        use syntect::parsing::syntax_definition::{Context, MatchOperation, MatchPattern, Pattern};
        let mut main = Context::new(false);
        main.patterns.push(Pattern::Match(MatchPattern::new(
            false,
            "FAIL".to_owned(),
            vec![],
            None,
            MatchOperation::Pop,
            None,
        )));
        main.patterns.push(Pattern::Match(MatchPattern::new(
            false,
            "ok".to_owned(),
            vec!["keyword.control".parse().unwrap()],
            None,
            MatchOperation::None,
            None,
        )));
        let mut builder = SyntaxSet::new().into_builder();
        builder.add(SyntaxDefinition {
            name: "Broken".to_owned(),
            file_extensions: vec!["broken".to_owned()],
            scope: "source.broken".parse().unwrap(),
            first_line_match: None,
            hidden: false,
            variables: Default::default(),
            contexts: [("__start".to_owned(), main)].into(),
        });
        let syntaxes = builder.build();
        let theme = Theme::system();
        let content = "ok\nFAIL\nafter\n";
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                highlight(
                    content,
                    language("broken", &syntaxes).unwrap(),
                    &syntaxes,
                    &theme,
                )
            }))
            .is_err(),
            "the broken grammar must exercise a real highlighter failure"
        );
        assert_eq!(
            render_with_syntaxes(
                content,
                "broken",
                &theme,
                &syntaxes,
                theme.markdown.code_block
            ),
            vec![
                Line::from(Span::styled("ok", theme.markdown.code_block)),
                Line::from(Span::styled("FAIL", theme.markdown.code_block)),
                Line::from(Span::styled("after", theme.markdown.code_block)),
            ]
        );
    }

    #[test]
    fn aliases_select_the_named_grammar_without_sniffing() {
        for (alias, name) in [
            ("js", "JavaScript"),
            ("ts", "TypeScript"),
            ("jsx", "TypeScriptReact"),
            ("javascriptreact", "TypeScriptReact"),
            ("sh", "Bourne Again Shell (bash)"),
            ("shellscript", "Bourne Again Shell (bash)"),
            ("py", "Python"),
            ("rs", "Rust"),
            ("csharp", "C#"),
            ("cs", "C#"),
            ("c#", "C#"),
            ("fsharp", "F#"),
            ("golang", "Go"),
            ("objc", "Objective-C"),
            ("objectivec", "Objective-C"),
            ("obj-c", "Objective-C"),
            ("cplusplus", "C++"),
            ("batch", "Batch File"),
            ("dos", "Batch File"),
            ("yml", "YAML"),
            ("dockerfile", "Dockerfile"),
        ] {
            assert_eq!(language(alias, &SYNTAXES).unwrap().name, name, "{alias}");
        }
        assert_eq!(language("RuSt title=demo", &SYNTAXES).unwrap().name, "Rust");
        assert!(language("", &SYNTAXES).is_none());
        assert!(language("not-a-language", &SYNTAXES).is_none());
    }

    #[test]
    fn scope_mapping_preserves_modifiers_and_feedback_colors() {
        let theme = Theme::system();
        for (scope, expected) in [
            (
                "comment.line.rust",
                theme.syntax.comment.add_modifier(Modifier::ITALIC),
            ),
            (
                "keyword.control.rust",
                theme.syntax.keyword.add_modifier(Modifier::ITALIC),
            ),
            (
                "entity.name.type.rust",
                theme.syntax.r#type.add_modifier(Modifier::BOLD),
            ),
            ("support.function.python", theme.feedback.error),
            ("variable.language.python", theme.feedback.error),
            ("entity.name.tag.html", theme.feedback.error),
            ("entity.other.attribute-name.html", theme.feedback.warning),
            ("meta.annotation.rust", theme.feedback.warning),
            ("string.quoted.double", theme.syntax.string),
            ("constant.numeric", theme.syntax.number),
            ("entity.name.function", theme.syntax.function),
            ("variable.parameter", theme.syntax.variable),
            ("keyword.operator", theme.syntax.operator),
            ("punctuation.section", theme.syntax.punctuation),
        ] {
            assert_eq!(scope_style(scope, &theme), Some(expected), "{scope}");
        }
        assert_eq!(scope_style("source.rust", &theme), None);
        assert_eq!(scope_style("stringlike", &theme), None);
    }
}
