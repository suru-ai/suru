use std::{
    ffi::OsString,
    io,
    ops::Range,
    process::{Command, Stdio},
};

use super::{clipboard::safe_hyperlink_target, text_layout::StyledSpan};

pub(super) fn open(target: &str) -> io::Result<()> {
    let target = safe_hyperlink_target(target)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsafe hyperlink target"))?;
    let (program, arguments) = opener(target);
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::thread::Builder::new()
        .name("suru-hyperlink-opener".to_owned())
        .spawn(move || {
            if let Err(error) = command.status() {
                tracing::warn!("hyperlink opener failed: {error}");
            }
        })
        .map(|_| ())
}

/// Gives every bare `http://` or `https://` URL among a line's spans the
/// URL as its target, so each row the line wraps onto opens the whole URL
/// rather than leaving the terminal to guess at the fragment it can see.
/// Only text no link already claims is searched: a run of adjacent such
/// spans is read as one, since Markdown splits a URL into several events
/// wherever it holds a would-be delimiter. Pass the line before it wraps.
pub(super) fn link_bare_urls(spans: &mut Vec<StyledSpan>) {
    let unclaimed = |span: &StyledSpan| !span.chrome && span.target.is_none();
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut offset = 0;
    let mut index = 0;
    while index < spans.len() {
        if !unclaimed(&spans[index]) {
            offset += spans[index].content.len();
            index += 1;
            continue;
        }
        let run_start = offset;
        let mut text = String::new();
        while index < spans.len() && unclaimed(&spans[index]) {
            text.push_str(&spans[index].content);
            index += 1;
        }
        offset += text.len();
        links.extend(bare_urls(&text).into_iter().map(|range| {
            let target = text[range.clone()].to_owned();
            (run_start + range.start..run_start + range.end, target)
        }));
    }
    if links.is_empty() {
        return;
    }
    let mut linked = Vec::with_capacity(spans.len() + links.len() * 2);
    let mut links = links.into_iter().peekable();
    let mut offset = 0;
    for span in spans.drain(..) {
        let span_start = offset;
        let span_end = offset + span.content.len();
        offset = span_end;
        let mut cursor = span_start;
        while cursor < span_end {
            while links.peek().is_some_and(|(range, _)| range.end <= cursor) {
                links.next();
            }
            let (end, target) = match links.peek() {
                Some((range, target)) if range.start <= cursor => {
                    (range.end.min(span_end), Some(target.clone()))
                }
                Some((range, _)) => (range.start.min(span_end), None),
                None => (span_end, None),
            };
            let mut piece = if cursor == span_start && end == span_end {
                span.clone()
            } else {
                span.slice(cursor - span_start..end - span_start)
            };
            if target.is_some() {
                piece.target = target;
            }
            linked.push(piece);
            cursor = end;
        }
    }
    *spans = linked;
}

/// The byte ranges of the bare `http://` and `https://` URLs in `text`, as
/// GitHub-flavored Markdown's extended autolinks read them: from a scheme
/// that does not continue a word, through to whitespace or `<`, less the
/// trailing punctuation that closes the sentence around it.
fn bare_urls(text: &str) -> Vec<Range<usize>> {
    let mut urls = Vec::new();
    let mut search = 0;
    while let Some(found) = text[search..].find("://") {
        let separator = search + found;
        let body = separator + "://".len();
        search = body;
        let Some(start) = ["https", "http"].into_iter().find_map(|scheme| {
            let start = separator.checked_sub(scheme.len())?;
            text.get(start..separator)?
                .eq_ignore_ascii_case(scheme)
                .then_some(start)
        }) else {
            continue;
        };
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        {
            continue;
        }
        let end = text[body..]
            .find(|character: char| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '<' | '>' | '"' | '`')
            })
            .map_or(text.len(), |length| body + length);
        let end = body + trimmed_url_length(&text[body..end]);
        if end > body && safe_hyperlink_target(&text[start..end]).is_some() {
            urls.push(start..end);
            search = end;
        }
    }
    urls
}

/// How much of a URL's body remains once the punctuation a sentence closes
/// it with is set aside: a final `.`, `,`, `!`, and their kin, and any
/// closing bracket the URL itself never opened.
fn trimmed_url_length(body: &str) -> usize {
    let mut body = body;
    loop {
        let Some(last) = body.chars().next_back() else {
            return 0;
        };
        let unbalanced = |open: char| body.matches(open).count() < body.matches(last).count();
        let trailing = match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '*' | '_' | '~' | '\'' => true,
            ')' => unbalanced('('),
            ']' => unbalanced('['),
            '}' => unbalanced('{'),
            _ => false,
        };
        if !trailing {
            return body.len();
        }
        body = &body[..body.len() - last.len_utf8()];
    }
}

fn opener(target: &str) -> (&'static str, Vec<OsString>) {
    #[cfg(target_os = "windows")]
    {
        (
            "rundll32",
            vec![OsString::from("url.dll,FileProtocolHandler"), target.into()],
        )
    }
    #[cfg(target_os = "macos")]
    {
        ("open", vec![OsString::from("--"), target.into()])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if target.starts_with('-') {
            format!("./{target}")
        } else {
            target.to_owned()
        };
        ("xdg-open", vec![target.into()])
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use crate::tui::text_layout::StyledSpan;

    fn urls(text: &str) -> Vec<&str> {
        super::bare_urls(text)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn bare_urls_end_where_the_sentence_around_them_resumes() {
        assert_eq!(
            urls("See https://example.test/a, then HTTP://example.test/b."),
            ["https://example.test/a", "HTTP://example.test/b"]
        );
        assert_eq!(
            urls("(see https://en.wikipedia.org/wiki/Rust_(language))"),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(
            urls("\"https://example.test/q?a=1&b=2\" <https://example.test/c>"),
            ["https://example.test/q?a=1&b=2", "https://example.test/c"]
        );
        assert_eq!(
            urls("https://example.test/a\nhttps://example.test/b"),
            ["https://example.test/a", "https://example.test/b"]
        );
    }

    #[test]
    fn only_a_web_scheme_that_begins_a_word_and_names_something_is_a_url() {
        assert!(urls("xhttps://example.test ftp://example.test file:///etc").is_empty());
        assert!(urls("bare https:// and https://.").is_empty());
        assert_eq!(urls("请看https://example.test"), ["https://example.test"]);
    }

    #[test]
    fn a_url_split_across_spans_links_each_part_and_nothing_around_it() {
        let style = Style::default().fg(Color::Indexed(3));
        let mut spans = vec![
            StyledSpan::chrome("  ", Style::default()),
            StyledSpan::text("go to https://example.test/a", style),
            StyledSpan::text("_b now", Style::default()),
        ];
        super::link_bare_urls(&mut spans);
        let url = Some("https://example.test/a_b");
        assert_eq!(
            spans
                .iter()
                .map(|span| (span.content.as_str(), span.style, span.target.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("  ", Style::default(), None),
                ("go to ", style, None),
                ("https://example.test/a", style, url),
                ("_b", Style::default(), url),
                (" now", Style::default(), None),
            ]
        );
    }

    #[test]
    fn text_a_link_already_claims_is_left_alone() {
        let mut spans = vec![
            StyledSpan::text("https://example.test/label", Style::default())
                .with_target(Some("https://example.test/elsewhere")),
            StyledSpan::chrome(" (https://example.test/chrome)", Style::default()),
        ];
        let before = spans.clone();
        super::link_bare_urls(&mut spans);
        assert_eq!(spans, before);
    }

    #[test]
    fn platform_opener_receives_the_target_as_one_literal_argument() {
        let target = "https://example.test/a?value=$(touch%20nope)&other=two words";
        let (_program, arguments) = super::opener(target);
        assert_eq!(arguments.last().and_then(|arg| arg.to_str()), Some(target));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_xdg_open_receives_no_unsupported_option_separator() {
        let (program, arguments) = super::opener("https://example.test");
        assert_eq!(program, "xdg-open");
        assert_eq!(
            arguments,
            [std::ffi::OsString::from("https://example.test")]
        );

        let (_, relative) = super::opener("-local-file");
        assert_eq!(relative, [std::ffi::OsString::from("./-local-file")]);
    }
}
