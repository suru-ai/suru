use pulldown_cmark::{CowStr, Event, LinkType, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Eq, PartialEq)]
enum HtmlTag {
    Strong,
    Emphasis,
    Link,
    Code,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Open {
    Html(HtmlTag),
    Markdown(TagEnd),
}

struct Normalizer {
    output: Vec<Event<'static>>,
    stack: Vec<Open>,
    code: Option<String>,
}

pub(super) fn normalize<'a>(events: impl IntoIterator<Item = Event<'a>>) -> Vec<Event<'static>> {
    let mut normalizer = Normalizer {
        output: Vec::new(),
        stack: Vec::new(),
        code: None,
    };
    for event in events {
        normalizer.event(event);
    }
    normalizer.finish()
}

impl Normalizer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::InlineHtml(raw) | Event::Html(raw) => self.tokenize(raw.as_ref()),
            Event::Start(Tag::HtmlBlock) => {
                self.open_markdown(Event::Start(Tag::Paragraph), TagEnd::Paragraph);
            }
            Event::End(TagEnd::HtmlBlock) => self.close_markdown(TagEnd::Paragraph),
            Event::Text(text) if self.code.is_some() => {
                self.code.as_mut().unwrap().push_str(text.as_ref());
            }
            Event::SoftBreak if self.code.is_some() => self.code.as_mut().unwrap().push(' '),
            Event::HardBreak if self.code.is_some() => self.code.as_mut().unwrap().push('\n'),
            Event::Code(text) if self.code.is_some() => {
                self.code.as_mut().unwrap().push_str(text.as_ref());
            }
            Event::FootnoteReference(label) if self.code.is_some() => {
                self.code.as_mut().unwrap().push_str(&format!("[^{label}]"));
            }
            Event::End(end) if self.stack.iter().any(|entry| *entry == Open::Markdown(end)) => {
                self.close_markdown(end);
            }
            Event::Start(_) if self.code.is_some() => {}
            Event::End(_) if self.code.is_some() => {}
            Event::Start(tag) => {
                let end = tag.to_end();
                self.open_markdown(Event::Start(tag.into_static()), end);
            }
            Event::End(end) => self.close_markdown(end),
            event => self.output.push(event.into_static()),
        }
    }

    fn finish(mut self) -> Vec<Event<'static>> {
        while let Some(entry) = self.stack.pop() {
            if let Open::Html(tag) = entry {
                self.close(tag);
            }
        }
        self.output
    }

    fn open_markdown(&mut self, event: Event<'static>, end: TagEnd) {
        self.output.push(event);
        self.stack.push(Open::Markdown(end));
    }

    fn close_markdown(&mut self, end: TagEnd) {
        while let Some(entry) = self.stack.pop() {
            match entry {
                Open::Html(tag) => self.close(tag),
                Open::Markdown(open) if open == end => break,
                Open::Markdown(open) => self.output.push(Event::End(open)),
            }
        }
        self.output.push(Event::End(end));
    }

    fn tokenize(&mut self, raw: &str) {
        let mut rest = raw;
        while let Some(start) = rest.find('<') {
            self.text(&rest[..start]);
            rest = &rest[start..];
            if rest.starts_with("<!--") {
                let Some(end) = rest.find("-->") else {
                    return;
                };
                rest = &rest[end + 3..];
                continue;
            }
            let Some(end) = tag_end(rest) else {
                self.text(rest);
                return;
            };
            let candidate = &rest[1..end];
            if !valid_tag(candidate) {
                self.text("<");
                rest = &rest[1..];
                continue;
            }
            self.tag(candidate);
            rest = &rest[end + 1..];
        }
        self.text(rest);
    }

    fn tag(&mut self, raw: &str) {
        let raw = raw.trim_end();
        if raw.starts_with('!') || raw.starts_with('?') {
            return;
        }
        let closing = raw.starts_with('/');
        let body = raw.trim_start_matches('/').trim_start();
        let name_end = body
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        if self.code.is_some() && !matches!(name.as_str(), "code" | "kbd") {
            if !closing
                && name == "img"
                && let Some(alt) = attribute(body, "alt")
            {
                self.code.as_mut().unwrap().push_str(&alt);
            }
            return;
        }
        if closing {
            self.close_named(&name);
            return;
        }
        match name.as_str() {
            "br" => self.output.push(Event::HardBreak),
            "b" | "strong" => self.open_html(HtmlTag::Strong, Event::Start(Tag::Strong)),
            "i" | "em" => self.open_html(HtmlTag::Emphasis, Event::Start(Tag::Emphasis)),
            "code" | "kbd" if self.code.is_none() => {
                self.code = Some(String::new());
                self.stack.push(Open::Html(HtmlTag::Code));
            }
            "a" => {
                let destination = attribute(body, "href").unwrap_or_default();
                self.stack.push(Open::Html(HtmlTag::Link));
                self.output.push(Event::Start(Tag::Link {
                    link_type: LinkType::Inline,
                    dest_url: CowStr::from(destination),
                    title: CowStr::from(String::new()),
                    id: CowStr::from(String::new()),
                }));
            }
            "img" => {
                let destination = attribute(body, "src").unwrap_or_default();
                let alt = attribute(body, "alt").unwrap_or_default();
                self.output.push(Event::Start(Tag::Image {
                    link_type: LinkType::Inline,
                    dest_url: CowStr::from(destination),
                    title: CowStr::from(String::new()),
                    id: CowStr::from(String::new()),
                }));
                self.output.push(Event::Text(CowStr::from(alt)));
                self.output.push(Event::End(TagEnd::Image));
            }
            _ => {}
        }
    }

    fn close_named(&mut self, name: &str) {
        let expected = match name {
            "b" | "strong" => Some(HtmlTag::Strong),
            "i" | "em" => Some(HtmlTag::Emphasis),
            "a" => Some(HtmlTag::Link),
            "code" | "kbd" => Some(HtmlTag::Code),
            _ => None,
        };
        let Some(expected) = expected else {
            return;
        };
        let Some(position) = self
            .stack
            .iter()
            .rposition(|entry| *entry == Open::Html(expected))
        else {
            return;
        };
        if !self.stack[position + 1..]
            .iter()
            .all(|entry| matches!(entry, Open::Html(_)))
        {
            return;
        }
        while self.stack.len() > position {
            let Open::Html(tag) = self.stack.pop().unwrap() else {
                unreachable!("Markdown scope was excluded above")
            };
            self.close(tag);
        }
    }

    fn open_html(&mut self, tag: HtmlTag, event: Event<'static>) {
        self.stack.push(Open::Html(tag));
        self.output.push(event);
    }

    fn close(&mut self, tag: HtmlTag) {
        match tag {
            HtmlTag::Strong => self.output.push(Event::End(TagEnd::Strong)),
            HtmlTag::Emphasis => self.output.push(Event::End(TagEnd::Emphasis)),
            HtmlTag::Link => self.output.push(Event::End(TagEnd::Link)),
            HtmlTag::Code => self.output.push(Event::Code(CowStr::from(
                self.code.take().unwrap_or_default(),
            ))),
        }
    }

    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        if let Some(code) = &mut self.code {
            code.push_str(&entities(raw));
        } else {
            for part in raw.split_inclusive('\n') {
                let content = part.strip_suffix('\n').unwrap_or(part);
                if !content.is_empty() {
                    self.output
                        .push(Event::Text(CowStr::from(entities(content))));
                }
                if part.ends_with('\n') {
                    self.output.push(Event::HardBreak);
                }
            }
        }
    }
}

fn valid_tag(raw: &str) -> bool {
    if raw
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("!doctype"))
    {
        return raw
            .as_bytes()
            .get(8)
            .is_none_or(|byte| byte.is_ascii_whitespace());
    }
    if raw.starts_with("![CDATA[") {
        return raw.ends_with("]]");
    }
    if let Some(instruction) = raw.strip_prefix('?') {
        return instruction.ends_with('?')
            && instruction
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic);
    }

    let bytes = raw.as_bytes();
    let closing = bytes.first() == Some(&b'/');
    let mut index = usize::from(closing);
    if !bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }
    index += 1;
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        index += 1;
    }
    if closing {
        return bytes[index..].iter().all(u8::is_ascii_whitespace);
    }

    loop {
        let before_space = index;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if index == bytes.len() {
            return true;
        }
        if bytes[index] == b'/' {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            return index == bytes.len();
        }
        if index == before_space {
            return false;
        }

        let name_start = index;
        while bytes.get(index).is_some_and(|byte| {
            !byte.is_ascii_whitespace() && !matches!(*byte, b'/' | b'=' | b'\'' | b'"' | b'<')
        }) {
            index += 1;
        }
        if index == name_start {
            return false;
        }
        let name_end = index;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            index = name_end;
            continue;
        }
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        let Some(&first) = bytes.get(index) else {
            return false;
        };
        if matches!(first, b'\'' | b'"') {
            index += 1;
            let Some(length) = bytes[index..].iter().position(|byte| *byte == first) else {
                return false;
            };
            index += length + 1;
        } else {
            let value_start = index;
            while bytes.get(index).is_some_and(|byte| {
                !byte.is_ascii_whitespace() && !matches!(*byte, b'\'' | b'"' | b'=' | b'<' | b'`')
            }) {
                index += 1;
            }
            if index == value_start {
                return false;
            }
        }
    }
}

fn tag_end(raw: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in raw.char_indices().skip(1) {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn attribute(body: &str, wanted: &str) -> Option<String> {
    let mut rest = &body[body.find(char::is_whitespace).unwrap_or(body.len())..];
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim_start();
        let (value, tail) = if let Some(quote @ ('\'' | '"')) = rest.chars().next() {
            let quoted = &rest[quote.len_utf8()..];
            let end = quoted.find(quote).unwrap_or(quoted.len());
            (
                &quoted[..end],
                &quoted[end.saturating_add(usize::from(end < quoted.len()))..],
            )
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(entities(value));
        }
        rest = tail;
    }
    None
}

fn entities(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = entity_end(rest) else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let source = &rest[..=end];
        let decoded = if source == "&apos;" {
            Some("'".to_owned())
        } else {
            let decoded = Parser::new(source)
                .filter_map(|event| match event {
                    Event::Text(text) => Some(text.into_string()),
                    _ => None,
                })
                .collect::<String>();
            (decoded != source).then_some(decoded)
        };
        output.push_str(decoded.as_deref().unwrap_or(source));
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

fn entity_end(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut index = 1;
    let valid = if bytes.get(index) == Some(&b'#') {
        index += 1;
        if matches!(bytes.get(index), Some(b'x' | b'X')) {
            index += 1;
            u8::is_ascii_hexdigit
        } else {
            u8::is_ascii_digit
        }
    } else {
        u8::is_ascii_alphanumeric
    };
    let start = index;
    while bytes.get(index).is_some_and(valid) {
        index += 1;
    }
    (index > start && bytes.get(index) == Some(&b';')).then_some(index)
}

#[cfg(test)]
mod tests {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    use super::{attribute, entities, normalize};

    #[test]
    fn entity_decoding_preserves_non_entities_and_resumes_after_bare_ampersands() {
        assert_eq!(
            entities("R&D with *stars*; and `ticks`; then &copy;"),
            "R&D with *stars*; and `ticks`; then ©"
        );
    }

    #[test]
    fn attribute_decoding_changes_only_valid_entities() {
        for (tag, name) in [
            (
                "a href='https://example.test/R&D?q=*stars*;&amp;copy=&copy;'",
                "href",
            ),
            (
                "img src='https://example.test/R&D?q=*stars*;&amp;copy=&copy;'",
                "src",
            ),
        ] {
            assert_eq!(
                attribute(tag, name).as_deref(),
                Some("https://example.test/R&D?q=*stars*;&copy=©")
            );
        }
    }

    #[test]
    fn code_html_is_one_balanced_atomic_event() {
        let events = normalize(Parser::new("<code><b>still</b></code>"));

        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], Event::Start(Tag::Paragraph)));
        assert!(matches!(&events[1], Event::Code(text) if text.as_ref() == "still"));
        assert!(matches!(events[2], Event::End(TagEnd::Paragraph)));
    }

    #[test]
    fn unclosed_html_is_closed_before_its_markdown_container() {
        let events = normalize(Parser::new("<b>bold\n\nplain"));

        assert!(matches!(events[0], Event::Start(Tag::Paragraph)));
        assert!(matches!(events[1], Event::Start(Tag::Strong)));
        assert!(matches!(events[3], Event::End(TagEnd::Strong)));
        assert!(matches!(events[4], Event::End(TagEnd::Paragraph)));
        assert!(matches!(events[5], Event::Start(Tag::Paragraph)));
    }

    #[test]
    fn html_is_closed_before_the_markdown_scope_that_contains_it() {
        let events = normalize(Parser::new("**<i>word** after"));

        assert!(matches!(events[1], Event::Start(Tag::Strong)));
        assert!(matches!(events[2], Event::Start(Tag::Emphasis)));
        assert!(matches!(events[4], Event::End(TagEnd::Emphasis)));
        assert!(matches!(events[5], Event::End(TagEnd::Strong)));
        assert!(matches!(&events[6], Event::Text(text) if text.as_ref() == " after"));
    }
}
