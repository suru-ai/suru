use pulldown_cmark::{CowStr, Event, LinkType, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Eq, PartialEq)]
enum OpenTag {
    Strong,
    Emphasis,
    Link,
    Code,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Open {
    Html(OpenTag),
    Markdown(TagEnd),
}

pub(super) fn normalize<'a>(events: impl IntoIterator<Item = Event<'a>>) -> Vec<Event<'static>> {
    let mut output = Vec::new();
    let mut stack = Vec::new();
    let mut code = None::<String>;
    for event in events {
        match event {
            Event::InlineHtml(raw) | Event::Html(raw) => {
                tokenize(raw.as_ref(), &mut output, &mut stack, &mut code);
            }
            Event::Start(Tag::HtmlBlock) => open_markdown(
                Event::Start(Tag::Paragraph),
                TagEnd::Paragraph,
                &mut output,
                &mut stack,
            ),
            Event::End(TagEnd::HtmlBlock) => {
                close_markdown(TagEnd::Paragraph, &mut output, &mut stack, &mut code);
            }
            Event::Text(text) if code.is_some() => code.as_mut().unwrap().push_str(text.as_ref()),
            Event::SoftBreak if code.is_some() => code.as_mut().unwrap().push(' '),
            Event::HardBreak if code.is_some() => code.as_mut().unwrap().push('\n'),
            Event::Code(text) if code.is_some() => code.as_mut().unwrap().push_str(text.as_ref()),
            Event::End(end) if stack.iter().any(|entry| *entry == Open::Markdown(end)) => {
                close_markdown(end, &mut output, &mut stack, &mut code);
            }
            Event::Start(_) if code.is_some() => {}
            Event::End(_) if code.is_some() => {}
            Event::Start(tag) => {
                let end = tag.to_end();
                open_markdown(
                    Event::Start(tag.into_static()),
                    end,
                    &mut output,
                    &mut stack,
                );
            }
            Event::End(end) => close_markdown(end, &mut output, &mut stack, &mut code),
            event => output.push(event.into_static()),
        }
    }
    while let Some(entry) = stack.pop() {
        if let Open::Html(tag) = entry {
            close(tag, &mut output, &mut code);
        }
    }
    output
}

fn open_markdown(
    event: Event<'static>,
    end: TagEnd,
    output: &mut Vec<Event<'static>>,
    stack: &mut Vec<Open>,
) {
    output.push(event);
    stack.push(Open::Markdown(end));
}

fn close_markdown(
    end: TagEnd,
    output: &mut Vec<Event<'static>>,
    stack: &mut Vec<Open>,
    code: &mut Option<String>,
) {
    while let Some(entry) = stack.pop() {
        match entry {
            Open::Html(tag) => close(tag, output, code),
            Open::Markdown(open) if open == end => break,
            Open::Markdown(open) => output.push(Event::End(open)),
        }
    }
    output.push(Event::End(end));
}

fn tokenize(
    raw: &str,
    output: &mut Vec<Event<'static>>,
    stack: &mut Vec<Open>,
    code: &mut Option<String>,
) {
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        text(&rest[..start], output, code);
        rest = &rest[start..];
        if rest.starts_with("<!--") {
            let Some(end) = rest.find("-->") else {
                return;
            };
            rest = &rest[end + 3..];
            continue;
        }
        let Some(end) = tag_end(rest) else {
            text(rest, output, code);
            return;
        };
        handle_tag(&rest[1..end], output, stack, code);
        rest = &rest[end + 1..];
    }
    text(rest, output, code);
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

fn handle_tag(
    raw: &str,
    output: &mut Vec<Event<'static>>,
    stack: &mut Vec<Open>,
    code: &mut Option<String>,
) {
    let raw = raw.trim();
    if raw.starts_with('!') || raw.starts_with('?') {
        return;
    }
    let closing = raw.starts_with('/');
    let body = raw.trim_start_matches('/').trim_start();
    let name_end = body
        .find(|c: char| c.is_ascii_whitespace() || c == '/')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    if code.is_some() && !matches!(name.as_str(), "code" | "kbd") {
        if !closing
            && name == "img"
            && let Some(alt) = attribute(body, "alt")
        {
            code.as_mut().unwrap().push_str(&alt);
        }
        return;
    }
    if closing {
        let expected = match name.as_str() {
            "b" | "strong" => Some(OpenTag::Strong),
            "i" | "em" => Some(OpenTag::Emphasis),
            "a" => Some(OpenTag::Link),
            "code" | "kbd" => Some(OpenTag::Code),
            _ => None,
        };
        if let Some(expected) = expected
            && let Some(position) = stack
                .iter()
                .rposition(|entry| *entry == Open::Html(expected))
            && stack[position + 1..]
                .iter()
                .all(|entry| matches!(entry, Open::Html(_)))
        {
            while stack.len() > position {
                let Open::Html(tag) = stack.pop().unwrap() else {
                    unreachable!("Markdown scope was excluded above")
                };
                close(tag, output, code);
            }
        }
        return;
    }
    match name.as_str() {
        "br" => output.push(Event::HardBreak),
        "b" | "strong" => open(OpenTag::Strong, Event::Start(Tag::Strong), output, stack),
        "i" | "em" => open(
            OpenTag::Emphasis,
            Event::Start(Tag::Emphasis),
            output,
            stack,
        ),
        "code" | "kbd" => {
            if code.is_none() {
                *code = Some(String::new());
                stack.push(Open::Html(OpenTag::Code));
            }
        }
        "a" => {
            let destination = attribute(body, "href").unwrap_or_default();
            stack.push(Open::Html(OpenTag::Link));
            output.push(Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url: CowStr::from(destination),
                title: CowStr::from(String::new()),
                id: CowStr::from(String::new()),
            }));
        }
        "img" => {
            let destination = attribute(body, "src").unwrap_or_default();
            let alt = attribute(body, "alt").unwrap_or_default();
            output.push(Event::Start(Tag::Image {
                link_type: LinkType::Inline,
                dest_url: CowStr::from(destination),
                title: CowStr::from(String::new()),
                id: CowStr::from(String::new()),
            }));
            output.push(Event::Text(CowStr::from(alt)));
            output.push(Event::End(TagEnd::Image));
        }
        _ => {}
    }
}

fn open(
    tag: OpenTag,
    event: Event<'static>,
    output: &mut Vec<Event<'static>>,
    stack: &mut Vec<Open>,
) {
    stack.push(Open::Html(tag));
    output.push(event);
}

fn close(tag: OpenTag, output: &mut Vec<Event<'static>>, code: &mut Option<String>) {
    match tag {
        OpenTag::Strong => output.push(Event::End(TagEnd::Strong)),
        OpenTag::Emphasis => output.push(Event::End(TagEnd::Emphasis)),
        OpenTag::Link => output.push(Event::End(TagEnd::Link)),
        OpenTag::Code => output.push(Event::Code(CowStr::from(code.take().unwrap_or_default()))),
    }
}

fn text(raw: &str, output: &mut Vec<Event<'static>>, code: &mut Option<String>) {
    if raw.is_empty() {
        return;
    }
    if let Some(code) = code {
        code.push_str(&entities(raw));
    } else {
        for part in raw.split_inclusive('\n') {
            let content = part.strip_suffix('\n').unwrap_or(part);
            if !content.is_empty() {
                output.push(Event::Text(CowStr::from(entities(content))));
            }
            if part.ends_with('\n') {
                output.push(Event::HardBreak);
            }
        }
    }
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
        let Some(end) = rest.find(';') else {
            output.push_str(rest);
            return output;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "apos" => Some("'".to_owned()),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|value| value.to_string())
            }
            value if value.starts_with('#') => value[1..]
                .parse()
                .ok()
                .and_then(char::from_u32)
                .map(|value| value.to_string()),
            _ => {
                let source = &rest[..=end];
                let decoded = Parser::new(source)
                    .filter_map(|event| match event {
                        Event::Text(text) => Some(text.into_string()),
                        _ => None,
                    })
                    .collect::<String>();
                (decoded != source).then_some(decoded)
            }
        };
        if let Some(decoded) = decoded {
            output.push_str(&decoded);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    use super::normalize;

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
