//! The paired representations of one selected fragment.
use pulldown_cmark::{Event, Parser, Tag, TagEnd, html};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardContent {
    pub text: String,
    pub html: Option<String>,
}

impl From<String> for ClipboardContent {
    fn from(text: String) -> Self {
        Self { text, html: None }
    }
}

impl From<&str> for ClipboardContent {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl ClipboardContent {
    pub(super) fn markdown(text: String, code_only: bool) -> Self {
        // Selection has always trimmed row ends. Apply that before producing
        // HTML too, especially for whitespace inside a selected Code Block.
        let text = text
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = String::new();
        if code_only {
            output.push_str("<pre><code>");
            html::push_html(
                &mut output,
                std::iter::once(Event::Text(text.as_str().into())),
            );
            output.push_str("</code></pre>\n");
        } else {
            let parser = Parser::new_ext(&text, super::markdown::options());
            let events = parser.map(|event| match event {
                // Only parser-generated structure is active markup.
                Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
                | Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => Event::Start(Tag::Link {
                    link_type,
                    dest_url: if safe_destination(&dest_url) {
                        dest_url
                    } else {
                        "".into()
                    },
                    title,
                    id,
                }),
                Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
                event => event,
            });
            html::push_html(&mut output, events);
        }
        Self {
            text,
            html: Some(output),
        }
    }

    pub(super) fn plain_html(&self) -> String {
        let mut output = String::from("<pre>");
        html::push_html(
            &mut output,
            std::iter::once(Event::Text(self.text.as_str().into())),
        );
        output.push_str("</pre>\n");
        output
    }
}

fn safe_destination(destination: &str) -> bool {
    let destination = destination.trim();
    if destination.chars().any(char::is_control) {
        return false;
    }
    let scheme = destination
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    match scheme.split_once(':') {
        Some((scheme, _)) => ["https", "http", "mailto"]
            .iter()
            .any(|safe| scheme.eq_ignore_ascii_case(safe)),
        None => true,
    }
}
