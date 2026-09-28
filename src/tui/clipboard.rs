//! The paired representations of one selected fragment, and what a reading
//! of the host clipboard for a paste finds there.
use pulldown_cmark::{Event, Parser, Tag, TagEnd, html};

/// One paste from the host clipboard, followed from the read the Application
/// asks for to the upload an image goes on to, so each answer reaches the
/// draft it was asked for.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PasteId(u64);

impl PasteId {
    pub(super) fn after(self) -> Self {
        Self(self.0 + 1)
    }
}

impl Default for PasteId {
    fn default() -> Self {
        Self(1)
    }
}

/// What reading the host clipboard for a paste found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardRead {
    /// An image, already encoded as PNG on this client.
    Image { png: Vec<u8> },
    /// Text, which pastes as a bracketed paste of it would.
    Text(String),
    /// Neither an image nor text.
    Empty,
    /// An image in a format that cannot be attached, named as readers know it.
    Unsupported { format: String },
    /// The clipboard could not be read, and why.
    Failed { reason: String },
}

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
    safe_destination_target(destination).is_some()
}

fn safe_destination_target(destination: &str) -> Option<&str> {
    let destination = destination.trim();
    if destination.chars().any(char::is_control) {
        return None;
    }
    let scheme = destination
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let safe = match scheme.split_once(':') {
        Some((scheme, _)) => ["https", "http", "mailto"]
            .iter()
            .any(|safe| scheme.eq_ignore_ascii_case(safe)),
        None => true,
    };
    safe.then_some(destination)
}

/// A destination the terminal and platform opener can act on without a base
/// URL. Relative Markdown links remain copyable but keep visible destination
/// chrome because the Session workspace, not Suru's process cwd, is their base.
pub(super) fn safe_hyperlink_target(destination: &str) -> Option<&str> {
    let destination = safe_destination_target(destination)?;
    let (scheme, _) = destination.split_once(':')?;
    ["https", "http", "mailto"]
        .iter()
        .any(|safe| scheme.eq_ignore_ascii_case(safe))
        .then_some(destination)
}
