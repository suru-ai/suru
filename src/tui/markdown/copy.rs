//! Selection metadata built from the very events the Markdown renderer paints.
//! Offsets address decoded event text, so escapes, entities and Unicode never
//! require a second guess at where a painted character came from.
use std::{collections::BTreeMap, ops::Range};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Tag};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceRange {
    node: usize,
    range: Range<usize>,
}

impl SourceRange {
    pub(crate) fn slice(&self, range: Range<usize>) -> Self {
        Self {
            node: self.node,
            range: self.range.start + range.start..self.range.start + range.end,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ColumnAlignment {
    None,
    Left,
    Center,
    Right,
}

impl From<Alignment> for ColumnAlignment {
    fn from(alignment: Alignment) -> Self {
        match alignment {
            Alignment::None => Self::None,
            Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

impl ColumnAlignment {
    pub(super) fn separator(&self) -> &'static str {
        match self {
            Self::None => "---",
            Self::Left => ":---",
            Self::Center => ":---:",
            Self::Right => "---:",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Kind {
    Root,
    Paragraph,
    Heading(usize),
    Quote,
    List(Option<u64>),
    Item,
    Emphasis,
    Strong,
    Link(String, String, bool),
    Code(String),
    InlineCode,
    Table(Vec<ColumnAlignment>),
    Head,
    Row,
    Cell,
    Text,
    Break,
    CellBoundary,
    Rule,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    kind: Kind,
    children: Vec<usize>,
    text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Document {
    nodes: Vec<Node>,
}

pub(super) struct Builder {
    document: Document,
    stack: Vec<usize>,
}

impl Builder {
    pub(super) fn new() -> Self {
        Self {
            document: Document {
                nodes: vec![Node {
                    kind: Kind::Root,
                    children: vec![],
                    text: String::new(),
                }],
            },
            stack: vec![0],
        }
    }

    fn add(&mut self, kind: Kind, text: &str) -> usize {
        let id = self.document.nodes.len();
        self.document.nodes.push(Node {
            kind,
            children: vec![],
            text: text.into(),
        });
        self.document.nodes[*self.stack.last().unwrap()]
            .children
            .push(id);
        id
    }

    pub(super) fn event(&mut self, event: &Event<'_>) -> Option<SourceRange> {
        let (kind, text) = match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Paragraph => Kind::Paragraph,
                    Tag::Heading { level, .. } => Kind::Heading(*level as usize),
                    Tag::BlockQuote(_) => Kind::Quote,
                    Tag::List(first) => Kind::List(*first),
                    Tag::Item => Kind::Item,
                    Tag::Emphasis => Kind::Emphasis,
                    Tag::Strong => Kind::Strong,
                    Tag::Link {
                        dest_url, title, ..
                    } => Kind::Link(dest_url.to_string(), title.to_string(), false),
                    Tag::Image {
                        dest_url, title, ..
                    } => Kind::Link(dest_url.to_string(), title.to_string(), true),
                    Tag::CodeBlock(kind) => Kind::Code(match kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    }),
                    Tag::Table(alignment) => Kind::Table(
                        alignment
                            .iter()
                            .copied()
                            .map(ColumnAlignment::from)
                            .collect(),
                    ),
                    Tag::TableHead => Kind::Head,
                    Tag::TableRow => Kind::Row,
                    Tag::TableCell => Kind::Cell,
                    _ => Kind::Other,
                };
                let id = self.add(kind, "");
                self.stack.push(id);
                return None;
            }
            Event::End(end) => {
                let source = if matches!(end, pulldown_cmark::TagEnd::TableCell) {
                    let node = self.add(Kind::CellBoundary, " | ");
                    Some(SourceRange { node, range: 0..3 })
                } else {
                    None
                };
                self.stack.pop();
                return source;
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                (Kind::Text, text.as_ref())
            }
            Event::Code(text) => (Kind::InlineCode, text.as_ref()),
            Event::SoftBreak => (Kind::Text, " "),
            Event::HardBreak => (Kind::Break, "\n"),
            Event::Rule => (Kind::Rule, "────────"),
            _ => return None,
        };
        let parent = *self.stack.last().unwrap();
        if matches!(self.document.nodes[parent].kind, Kind::Code(_)) {
            let node = &mut self.document.nodes[parent];
            let start = node.text.len();
            node.text.push_str(text);
            return Some(SourceRange {
                node: parent,
                range: start..node.text.len(),
            });
        }
        let node = self.add(kind, text);
        Some(SourceRange {
            node,
            range: 0..text.len(),
        })
    }

    pub(super) fn finish(self) -> Document {
        self.document
    }
}

impl Document {
    pub(crate) fn copy(&self, ranges: &[SourceRange], single_content: bool) -> String {
        let mut selected: BTreeMap<usize, Vec<Range<usize>>> = BTreeMap::new();
        for source in ranges {
            if !source.range.is_empty() {
                selected
                    .entry(source.node)
                    .or_default()
                    .push(source.range.clone());
            }
        }
        let code_only = single_content
            && selected.len() == 1
            && selected
                .keys()
                .next()
                .is_some_and(|id| matches!(self.nodes[*id].kind, Kind::Code(_)));
        let root = if code_only {
            *selected.first_key_value().unwrap().0
        } else {
            0
        };
        self.fragment(root, &selected, code_only)
            .unwrap_or_default()
    }

    fn fragment(
        &self,
        id: usize,
        selected: &BTreeMap<usize, Vec<Range<usize>>>,
        code_only: bool,
    ) -> Option<String> {
        let node = &self.nodes[id];
        if matches!(node.kind, Kind::Break)
            && selected
                .first_key_value()
                .is_some_and(|(first, _)| *first < id)
            && selected
                .last_key_value()
                .is_some_and(|(last, _)| id < *last)
        {
            return Some("\\\n".into());
        }
        if let Some(ranges) = selected.get(&id) {
            let mut text = String::new();
            let mut end = ranges[0].start;
            for range in ranges {
                // Restore line feeds between visible code rows, never text
                // from a hidden interval. Soft wrapping already keeps offsets.
                if range.start > end && node.text[end..range.start].chars().all(char::is_whitespace)
                {
                    text.push_str(&node.text[end..range.start]);
                }
                text.push_str(&node.text[range.start.max(end)..range.end]);
                end = range.end;
            }
            return Some(match &node.kind {
                Kind::Code(info) if !code_only => {
                    let marker = if info.contains('`') { '~' } else { '`' };
                    let fence = marker
                        .to_string()
                        .repeat(delimiter_run(&text, marker).max(2) + 1);
                    let info = metadata(info);
                    format!("{fence}{info}\n{}\n{fence}", text.trim_end_matches('\n'))
                }
                Kind::Code(_) => text,
                Kind::InlineCode => inline_code(&text),
                Kind::Rule => "---".into(),
                Kind::CellBoundary => String::new(),
                Kind::Break => "  \n".into(),
                _ => escape(&text),
            });
        }
        let children = node
            .children
            .iter()
            .filter_map(|child| {
                self.fragment(*child, selected, code_only)
                    .map(|text| (*child, text))
            })
            .collect::<Vec<_>>();
        if children.is_empty() {
            return None;
        }
        let inline = || {
            children
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>()
        };
        let blocks = || {
            children
                .iter()
                .map(|(_, text)| text.trim_end())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        Some(match &node.kind {
            Kind::Root | Kind::Quote => {
                let text = blocks();
                if matches!(node.kind, Kind::Quote) {
                    text.lines()
                        .map(|line| format!("> {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    text
                }
            }
            Kind::Item => {
                let mut text = String::new();
                for (child, fragment) in &children {
                    match self.nodes[*child].kind {
                        Kind::List(_) => text.push('\n'),
                        Kind::Paragraph
                        | Kind::Code(_)
                        | Kind::Quote
                        | Kind::Heading(_)
                        | Kind::Table(_)
                        | Kind::Rule
                            if !text.is_empty() =>
                        {
                            text.push_str("\n\n")
                        }
                        _ => {}
                    }
                    text.push_str(fragment);
                }
                // Tight list items have direct inline children; loose items
                // contain Paragraphs that already protect their own text.
                // Never escape syntax supplied by a selected block child.
                if matches!(
                    self.nodes[children[0].0].kind,
                    Kind::Text | Kind::Emphasis | Kind::Strong | Kind::Link(..) | Kind::InlineCode
                ) {
                    protect_block_start(text)
                } else {
                    text
                }
            }
            Kind::Paragraph => protect_block_start(inline()),
            Kind::List(first) => children
                .iter()
                .map(|(child, text)| {
                    let position = node.children.iter().position(|id| id == child).unwrap();
                    let marker = first.map_or_else(
                        || "- ".into(),
                        |first| format!("{}. ", first.saturating_add(position as u64)),
                    );
                    let indent = " ".repeat(marker.len());
                    format!("{marker}{}", text.replace('\n', &format!("\n{indent}")))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Kind::Heading(level) => format!("{} {}", "#".repeat(*level), inline()),
            Kind::Emphasis => wrap(&inline(), "*", "*"),
            Kind::Strong => wrap(&inline(), "**", "**"),
            Kind::Link(url, title, image) => {
                let title = if title.is_empty() {
                    String::new()
                } else {
                    format!(" \"{}\"", metadata(title).replace('"', "\\\""))
                };
                let destination = if url
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>'))
                {
                    format!(
                        "<{}>",
                        metadata(url).replace('>', "%3E").replace('<', "%3C")
                    )
                } else {
                    metadata(url)
                };
                wrap(
                    &inline(),
                    if *image { "![" } else { "[" },
                    &format!("]({destination}{title})"),
                )
            }
            Kind::Table(alignments) => self.table(node, alignments, selected, code_only),
            _ => inline(),
        })
    }

    fn table(
        &self,
        table: &Node,
        alignments: &[ColumnAlignment],
        selected: &BTreeMap<usize, Vec<Range<usize>>>,
        code_only: bool,
    ) -> String {
        let rows = table
            .children
            .iter()
            .filter_map(|row| {
                let cells = self.nodes[*row]
                    .children
                    .iter()
                    .enumerate()
                    .filter_map(|(column, cell)| {
                        self.fragment(*cell, selected, code_only)
                            .map(|text| (column, table_pipes(&text)))
                    })
                    .collect::<Vec<_>>();
                (!cells.is_empty()).then_some((*row, cells))
            })
            .collect::<Vec<_>>();
        let first = rows
            .iter()
            .flat_map(|(_, cells)| cells.iter().map(|(column, _)| *column))
            .min()
            .unwrap_or(0);
        let last = rows
            .iter()
            .flat_map(|(_, cells)| cells.iter().map(|(column, _)| *column))
            .max()
            .unwrap_or(first);
        let row_text = |cells: &[(usize, String)]| {
            format!(
                "| {} |",
                (first..=last)
                    .map(|column| cells
                        .iter()
                        .find(|(index, _)| *index == column)
                        .map_or("", |(_, text)| text.trim()))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        };
        let mut output = Vec::new();
        let has_head = rows
            .first()
            .is_some_and(|(id, _)| matches!(self.nodes[*id].kind, Kind::Head));
        output.push(if has_head {
            row_text(&rows[0].1)
        } else {
            row_text(&[])
        });
        output.push(format!(
            "| {} |",
            (first..=last)
                .map(|column| alignments
                    .get(column)
                    .map_or("---", ColumnAlignment::separator))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        output.extend(
            rows.iter()
                .skip(usize::from(has_head))
                .map(|(_, cells)| row_text(cells)),
        );
        output.join("\n")
    }
}

fn delimiter_run(text: &str, marker: char) -> usize {
    text.split(|c| c != marker).map(str::len).max().unwrap_or(0)
}
fn inline_code(text: &str) -> String {
    let fence = "`".repeat(delimiter_run(text, '`') + 1);
    let padding = if text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' ') && !text.trim().is_empty())
    {
        " "
    } else {
        ""
    };
    format!("{fence}{padding}{text}{padding}{fence}")
}
fn wrap(text: &str, open: &str, close: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.into();
    }
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    format!("{}{open}{trimmed}{close}{}", &text[..start], &text[end..])
}
fn escape(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '*' | '_' | '[' | ']' | '`' | '|' | '#' | '>' | '<' | '&'
        ) {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

// GFM recognizes a pipe as a cell separator even inside a code span.
fn table_pipes(text: &str) -> String {
    let mut output = String::new();
    let mut backslashes = 0;
    for character in text.chars() {
        if character == '|' && backslashes % 2 == 0 {
            output.push('\\');
        }
        output.push(character);
        backslashes = if character == '\\' {
            backslashes + 1
        } else {
            0
        };
    }
    output
}

/// Decoded destinations, titles and fence info are parsed for escapes and
/// entities again when pasted. Preserve their value across that second parse.
fn metadata(text: &str) -> String {
    text.replace('&', "&amp;").replace('\\', "\\\\")
}

/// A partial paragraph may begin at punctuation that was harmless mid-line.
/// Keep that text from turning into a list, thematic break, or setext heading.
fn protect_block_start(mut text: String) -> String {
    let leading = text.len() - text.trim_start_matches(' ').len();
    let content = &text[leading..];
    let digits = content.bytes().take_while(u8::is_ascii_digit).count();
    let marker = if content.starts_with(['-', '+', '=', '~']) {
        Some(0)
    } else if (1..=9).contains(&digits)
        && matches!(content.as_bytes().get(digits), Some(b'.' | b')'))
        && content
            .as_bytes()
            .get(digits + 1)
            .is_none_or(u8::is_ascii_whitespace)
    {
        Some(digits)
    } else {
        None
    };
    if let Some(marker) = marker {
        text.insert(leading + marker, '\\');
    }
    text
}
