//! Where text breaks across the rows a surface draws it in.
//!
//! The composer reads [`TextLayout`] for its caret, its block height, and the
//! lines a frame draws — all three have to agree on one wrap rather than each
//! deriving its own — and the Transcript reads it for the user Messages it
//! draws. Rows break on whole words: a word that does not fit the room left on
//! a row moves to the next row entire, and only a word too wide for a row of
//! its own is split within.
//!
//! The Transcript wraps every other projected line through [`StyledLayout`],
//! the same idea extended to styled lines (ADR 0020): each row records the
//! byte range of the line it draws, so a screen cell resolves to a character
//! offset through the wrap that put it there. Its break rules are the ones
//! the Transcript's rows have always followed — ratatui's untrimmed word wrap,
//! plus a hanging indent beneath leading whitespace and Markdown markers —
//! kept rather than [`TextLayout`]'s so that owning the wrap changed no row.

use std::{collections::VecDeque, ops::Range};

use ratatui::{
    buffer::Buffer,
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One visual row: where its text begins in the whole text, the text it draws,
/// and the columns that text occupies.
pub(super) struct LaidOutRow<'a> {
    pub(super) start: usize,
    pub(super) text: &'a str,
    pub(super) width: usize,
}

/// An insertion offset and which side of a visual wrap the pointer chose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorTarget {
    pub(super) offset: usize,
    pub(super) prefer_previous_row: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowDirection {
    Previous,
    Next,
}

/// Text laid out over content columns `width` wide, as the byte range of each
/// visual row.
pub(super) struct TextLayout<'a> {
    text: &'a str,
    width: usize,
    rows: Vec<Range<usize>>,
}

impl<'a> TextLayout<'a> {
    pub(super) fn new(text: &'a str, width: u16) -> Self {
        let width = usize::from(width.max(1));
        let mut rows = Vec::new();
        let mut row_start = 0_usize;
        let mut column = 0_usize;
        let mut offset = 0_usize;
        while let Some(character) = text[offset..].chars().next() {
            if character == '\n' {
                rows.push(row_start..offset);
                offset += character.len_utf8();
                row_start = offset;
                column = 0;
                continue;
            }
            if character.is_whitespace() {
                // Whitespace never starts a row of its own: the space that ends
                // a row stays with the row it ended, so a word moved down lands
                // at the left edge rather than a column in from it. A row can
                // run past `width` that way, and the columns past the edge are
                // blanks nobody sees.
                column += display_width(character);
                offset += character.len_utf8();
                continue;
            }
            let word = word_at(text, offset);
            let word_width = text[word.clone()].chars().map(display_width).sum::<usize>();
            if column > 0 && column + word_width > width {
                rows.push(row_start..offset);
                row_start = offset;
                column = 0;
            }
            if word_width <= width {
                column += word_width;
                offset = word.end;
                continue;
            }
            // The word is wider than a row of its own, so there is nowhere to
            // move it to: it fills rows character by character instead.
            for (character_offset, character) in text[word.clone()].char_indices() {
                let character_offset = word.start + character_offset;
                let character_width = display_width(character);
                if column > 0 && column + character_width > width {
                    rows.push(row_start..character_offset);
                    row_start = character_offset;
                    column = 0;
                }
                column += character_width;
            }
            offset = word.end;
        }
        rows.push(row_start..text.len());
        Self { text, width, rows }
    }

    /// Each visual row in order.
    pub(super) fn rows(&self) -> impl Iterator<Item = LaidOutRow<'a>> + '_ {
        self.rows.iter().map(|row| self.drawn(row))
    }

    /// What a row shows: the whitespace that ran past the layout's columns is
    /// left out, since none of it shows, so a row is never wider than those
    /// columns — save a lone character too wide for a row of its own, which is
    /// kept rather than dropped, having nowhere narrower to go.
    fn drawn(&self, row: &Range<usize>) -> LaidOutRow<'a> {
        let text = &self.text[row.clone()];
        let mut width = 0_usize;
        for (offset, character) in text.char_indices() {
            let character_width = display_width(character);
            if width > 0 && width + character_width > self.width {
                return LaidOutRow {
                    start: row.start,
                    text: &text[..offset],
                    width,
                };
            }
            width += character_width;
        }
        LaidOutRow {
            start: row.start,
            text,
            width,
        }
    }

    /// The insertion offset at a displayed cell. Both cells of a wide
    /// character point before it; blank space points to the row's end.
    pub(super) fn cursor_target(&self, row: u16, column: u16) -> CursorTarget {
        let Some(range) = self.rows.get(usize::from(row)) else {
            return CursorTarget {
                offset: self.text.len(),
                prefer_previous_row: false,
            };
        };
        let mut width = 0;
        for (offset, character) in self.text[range.clone()].char_indices() {
            width += display_width(character);
            if usize::from(column) < width {
                return CursorTarget {
                    offset: range.start + offset,
                    prefer_previous_row: false,
                };
            }
        }
        CursorTarget {
            offset: range.end,
            prefer_previous_row: self.cursor_position(range.end).0 > row,
        }
    }

    /// A pointer can choose the end of a soft-wrapped row rather than the
    /// next row's start, even though both share the same insertion offset.
    pub(super) fn cursor_position_before_wrap(&self, cursor: usize) -> (u16, u16) {
        if let Some((index, row)) = self
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.end == cursor && self.drawn(row).width < self.width)
        {
            return (
                u16::try_from(index).unwrap_or(u16::MAX),
                self.drawn(row).width as u16,
            );
        }
        self.cursor_position(cursor)
    }

    /// Where a caret with an explicit wrap-side affinity is painted.
    pub(super) fn cursor_position_with_affinity(
        &self,
        cursor: usize,
        prefer_previous_row: bool,
    ) -> (u16, u16) {
        if prefer_previous_row {
            self.cursor_position_before_wrap(cursor)
        } else {
            self.cursor_position(cursor)
        }
    }

    /// The nearest insertion point at `column` on the Row adjacent to the
    /// caret. The returned affinity keeps a soft-wrap boundary on the Row it
    /// was reached from, while an exact-width trailing caret remains its own
    /// Row below the text.
    pub(super) fn adjacent_cursor_target(
        &self,
        cursor: usize,
        prefer_previous_row: bool,
        column: u16,
        direction: RowDirection,
    ) -> Option<CursorTarget> {
        let current_row = self
            .cursor_position_with_affinity(cursor, prefer_previous_row)
            .0;
        let target_row = match direction {
            RowDirection::Previous => current_row.checked_sub(1)?,
            RowDirection::Next => current_row.checked_add(1)?,
        };
        let last_row = self
            .row_count()
            .saturating_sub(1)
            .max(self.cursor_position(self.text.len()).0);
        if target_row > last_row {
            return None;
        }

        let Some(range) = self.rows.get(usize::from(target_row)) else {
            return Some(CursorTarget {
                offset: self.text.len(),
                prefer_previous_row: false,
            });
        };
        let drawn = self.drawn(range);
        let mut best = (column, range.start, false);
        let mut display_column = 0_u16;
        for (relative_offset, character) in drawn.text.char_indices() {
            display_column = display_column
                .saturating_add(u16::try_from(display_width(character)).unwrap_or(u16::MAX));
            let offset = range.start + relative_offset + character.len_utf8();
            let at_row_end = offset == range.end;
            let next_row_shares_offset = self
                .rows
                .get(usize::from(target_row).saturating_add(1))
                .is_some_and(|next| next.start == offset);
            let newline_ended = self.text[offset..].starts_with('\n');
            let (is_on_row, affinity) = if !at_row_end {
                (usize::from(display_column) < self.width, false)
            } else if next_row_shares_offset {
                (usize::from(display_column) < self.width, true)
            } else if newline_ended {
                (true, false)
            } else {
                (usize::from(display_column) < self.width, false)
            };
            if !is_on_row {
                continue;
            }
            let distance = display_column.abs_diff(column);
            if distance < best.0 {
                best = (distance, offset, affinity);
            }
        }
        Some(CursorTarget {
            offset: best.1,
            prefer_previous_row: best.2,
        })
    }

    /// How many rows the text occupies.
    pub(super) fn row_count(&self) -> u16 {
        u16::try_from(self.rows.len()).unwrap_or(u16::MAX)
    }

    /// Where the caret sits for a byte offset into the text, as a row and a
    /// column. An offset that lands at a wrapped row's right edge spills to the
    /// start of the row below, which is where the next character typed goes;
    /// one at the right edge of a row a newline ended waits in that row's
    /// margin instead, since the row below holds a line of its own.
    pub(super) fn cursor_position(&self, cursor: usize) -> (u16, u16) {
        let row_index = self
            .rows
            .iter()
            .rposition(|row| row.start <= cursor)
            .unwrap_or(0);
        let row = &self.rows[row_index];
        let end = cursor.clamp(row.start, row.end);
        let column = self.text[row.start..end]
            .chars()
            .map(display_width)
            .sum::<usize>()
            .min(self.width);
        let row_index = u16::try_from(row_index).unwrap_or(u16::MAX);
        let newline_ended = self.text[row.end..].starts_with('\n');
        if column >= self.width && !newline_ended {
            (row_index.saturating_add(1), 0)
        } else {
            (row_index, u16::try_from(column).unwrap_or(u16::MAX))
        }
    }
}

/// A run of a projected line drawn in one style. Chrome is decoration Suru
/// draws around the text rather than the text itself — a Marker, an indent, a
/// Fold affordance, a Code Block border, a Command prefix — so a copy of the
/// line can leave it out while the draw keeps it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StyledSpan {
    pub(super) content: String,
    pub(super) style: Style,
    pub(super) chrome: bool,
    /// The decoded Markdown event bytes painted by this span, when authored
    /// as Markdown. Slicing a span slices these offsets along with its text.
    pub(super) source: Option<super::markdown::copy::SourceRange>,
}

impl StyledSpan {
    pub(super) fn text(content: impl Into<String>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
            chrome: false,
            source: None,
        }
    }

    pub(super) fn chrome(content: impl Into<String>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
            chrome: true,
            source: None,
        }
    }

    pub(super) fn width(&self) -> usize {
        self.content.width()
    }
}

/// A projected line: the spans it draws, and whether it continues a line the
/// projection split for being too long to wrap whole. A continuation is the
/// same written line as the one before it, so a copy joins the two without a
/// line feed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct StyledLine {
    pub(super) spans: Vec<StyledSpan>,
    pub(super) continuation: bool,
    /// Source whitespace omitted between this continuation and the preceding
    /// prewrapped row. Copy restores it only when joining those rows.
    pub(super) omitted_prefix: String,
    /// Shared across the lines of one Markdown projection, including blank
    /// lines. Its identity keeps selected Messages and Reasoning blocks apart.
    pub(super) markdown: Option<std::sync::Arc<super::markdown::copy::Document>>,
}

impl StyledLine {
    /// A line of one span of text.
    /// Read by the projection's tests until a renderer needs a bare text line.
    #[allow(dead_code)]
    pub(super) fn text(content: impl Into<String>, style: Style) -> Self {
        Self::from(vec![StyledSpan::text(content, style)])
    }

    /// A line of one span of chrome.
    pub(super) fn chrome(content: impl Into<String>, style: Style) -> Self {
        Self::from(vec![StyledSpan::chrome(content, style)])
    }

    /// Everything the line draws, chrome included, as one string. Offsets a
    /// row reports are byte offsets into this.
    pub(super) fn written_text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    }

    pub(super) fn width(&self) -> usize {
        self.spans.iter().map(StyledSpan::width).sum()
    }

    /// Read by the projection's tests until the Text Selection lands.
    #[allow(dead_code)]
    pub(super) fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The part of the line between two byte offsets into its text, its
    /// spans cut at the offsets and each keeping its style and chrome.
    pub(super) fn slice(&self, range: Range<usize>) -> Self {
        let mut spans = Vec::new();
        let mut offset = 0;
        for span in &self.spans {
            let span_range = offset..offset + span.content.len();
            offset = span_range.end;
            let start = range.start.max(span_range.start);
            let end = range.end.min(span_range.end);
            if start < end {
                spans.push(StyledSpan {
                    content: span.content[start - span_range.start..end - span_range.start]
                        .to_owned(),
                    style: span.style,
                    chrome: span.chrome,
                    source: span.source.as_ref().map(|source| {
                        source.slice(start - span_range.start..end - span_range.start)
                    }),
                });
            }
        }
        Self {
            spans,
            continuation: self.continuation,
            markdown: self.markdown.clone(),
            omitted_prefix: if range.start == 0 {
                self.omitted_prefix.clone()
            } else {
                String::new()
            },
        }
    }

    /// The line as ratatui draws it, its chrome indistinguishable from text.
    pub(super) fn to_line(&self) -> Line<'static> {
        Line::from(
            self.spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), span.style))
                .collect::<Vec<_>>(),
        )
    }
}

/// A ratatui line read as text throughout, its line style folded into each
/// span, for content that arrives already laid out as ratatui spans.
impl From<Line<'static>> for StyledLine {
    fn from(line: Line<'static>) -> Self {
        let style = line.style;
        Self::from(
            line.spans
                .into_iter()
                .map(|span| StyledSpan::text(span.content.into_owned(), style.patch(span.style)))
                .collect::<Vec<_>>(),
        )
    }
}

impl From<Vec<StyledSpan>> for StyledLine {
    fn from(spans: Vec<StyledSpan>) -> Self {
        Self {
            spans,
            continuation: false,
            omitted_prefix: String::new(),
            markdown: None,
        }
    }
}

/// One row a styled line wraps to: the byte range of the line's text it
/// draws, the hanging indent drawn before that text, and the row as drawn.
/// The range is what a screen cell resolves back through, so it comes from
/// the same wrap that decided the row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StyledRow {
    /// Byte offset into the line's text where the row's text begins.
    pub(super) start: usize,
    /// Byte offset past the row's last drawn character. Whitespace the wrap
    /// dropped at the break lies between one row's end and the next one's
    /// start.
    pub(super) end: usize,
    /// Columns of hanging indent before the row's text.
    pub(super) indent: usize,
    pub(super) line: Line<'static>,
}

impl StyledRow {
    /// The byte boundary after a selected cell, including a whole wide glyph
    /// but never the first glyph of the next wrapped row.
    pub(super) fn offset_after(&self, line: &StyledLine, width: u16, column: usize) -> usize {
        let offset = self.offset_at(line, width, column);
        if offset == self.end || column < self.indent {
            return offset;
        }
        let mut span_start = 0;
        for span in &line.spans {
            let span_end = span_start + span.content.len();
            if offset < span_end {
                return offset
                    + span.content[offset - span_start..]
                        .graphemes(true)
                        .next()
                        .map_or(0, str::len);
            }
            span_start = span_end;
        }
        offset
    }

    /// The byte offset in `line`'s text that column `column` of this row
    /// lands on. A column in the hanging indent answers where the row's text
    /// begins; both cells of a wide character answer that character; a column
    /// past the row's text answers the offset past its last character.
    pub(super) fn offset_at(&self, line: &StyledLine, width: u16, column: usize) -> usize {
        let Some(column) = column.checked_sub(self.indent) else {
            return self.start;
        };
        let maximum = usize::from(width);
        let mut used = 0;
        // Symbols are read span by span, as the wrap read them, so a cluster
        // that straddles two spans resolves at the boundary the wrap used.
        let mut span_start = 0;
        for span in &line.spans {
            let span_end = span_start + span.content.len();
            let from = self.start.clamp(span_start, span_end) - span_start;
            let to = self.end.clamp(span_start, span_end) - span_start;
            for (offset, symbol) in span.content[from..to].grapheme_indices(true) {
                let symbol_width = symbol.width();
                if symbol_width > maximum {
                    // The wrap drew nothing for a symbol wider than a row.
                    continue;
                }
                if column < used + symbol_width {
                    return span_start + from + offset;
                }
                used += symbol_width;
            }
            span_start = span_end;
        }
        self.end
    }
}

/// A styled line laid out over `width` columns: the rows it wraps to. Rows
/// break the way ratatui's untrimmed word wrap breaks them, which is what the
/// Transcript has always drawn with, plus one extension: once the first row
/// fills, every later row hangs beneath the line's leading whitespace and
/// Markdown structural markers (see [`continuation_prefix`]).
pub(super) struct StyledLayout {
    rows: Vec<StyledRow>,
}

impl StyledLayout {
    pub(super) fn new(line: &StyledLine, width: u16) -> Self {
        let text_len = line.spans.iter().map(|span| span.content.len()).sum();
        // A line whose display width fits the row is one row as it stands,
        // so the wrap only runs for lines that wrap. A span-embedded newline
        // and whitespace-only content both fall through to the wrap, whose
        // rules for them are not what the width sum sees.
        if width > 0
            && line.width() <= usize::from(width)
            && line.spans.iter().all(|span| !span.content.contains('\n'))
            && (line.width() == 0
                || line.spans.iter().any(|span| {
                    span.content
                        .chars()
                        .any(|character| !character.is_whitespace())
                }))
        {
            return Self {
                rows: vec![StyledRow {
                    start: 0,
                    end: text_len,
                    indent: 0,
                    line: line.to_line(),
                }],
            };
        }
        let symbols = styled_symbols(line);
        let prefix = continuation_prefix(&symbols, width);
        Self {
            rows: wrap_with_continuation_indent(symbols, text_len, width, &prefix),
        }
    }

    /// Read by the layout's tests; the projection takes its rows by value.
    #[allow(dead_code)]
    pub(super) fn rows(&self) -> &[StyledRow] {
        &self.rows
    }

    pub(super) fn into_rows(self) -> Vec<StyledRow> {
        self.rows
    }

    pub(super) fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// Draws one laid-out row into `buffer` from a cell over `width` columns, the
/// way ratatui's paragraph draws a wrapped row: grapheme by grapheme,
/// zero-width symbols skipped, and nothing painted past the row's last
/// symbol.
pub(super) fn draw_row(buffer: &mut Buffer, x: u16, y: u16, width: u16, line: &Line<'static>) {
    let mut column = x;
    let right = x.saturating_add(width).min(buffer.area.right());
    for span in &line.spans {
        let style = line.style.patch(span.style);
        for symbol in span.content.graphemes(true) {
            let symbol_width = symbol.width();
            if symbol_width == 0 {
                continue;
            }
            if column >= right {
                return;
            }
            buffer[(column, y)].set_symbol(symbol).set_style(style);
            column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX));
        }
    }
}

#[derive(Clone, Debug)]
struct StyledSymbol {
    symbol: String,
    style: Style,
    /// Byte offset of the symbol in its line's text, or `None` for a symbol
    /// of hanging indent, which the line's text does not hold.
    offset: Option<usize>,
}

impl StyledSymbol {
    fn width(&self) -> usize {
        self.symbol.width()
    }

    fn is_whitespace(&self) -> bool {
        self.symbol == "\u{200b}"
            || (self.symbol != "\u{00a0}" && self.symbol.chars().all(char::is_whitespace))
    }
}

fn styled_symbols(line: &StyledLine) -> Vec<StyledSymbol> {
    let mut offset = 0;
    line.spans
        .iter()
        .flat_map(|span| {
            let span_start = offset;
            offset += span.content.len();
            UnicodeSegmentation::grapheme_indices(span.content.as_str(), true).map(
                move |(index, symbol)| StyledSymbol {
                    symbol: symbol.to_owned(),
                    style: span.style,
                    offset: Some(span_start + index),
                },
            )
        })
        .collect()
}

/// The whitespace repeated before every wrapped continuation of a projected
/// line. Ordinary leading whitespace repeats verbatim. Markdown structural
/// markers become same-width spaces as well, giving list items and quotes a
/// hanging indent beneath the text rather than beneath the marker.
fn continuation_prefix(symbols: &[StyledSymbol], width: u16) -> Vec<StyledSymbol> {
    let leading_end = symbols
        .iter()
        .position(|symbol| !symbol.is_whitespace())
        .unwrap_or(symbols.len());
    let mut prefix_end = leading_end;
    while let Some(marker_end) = structural_marker_end(symbols, prefix_end) {
        prefix_end = marker_end;
    }
    if prefix_end == 0 || width <= 1 {
        return Vec::new();
    }

    let maximum = usize::from(width.saturating_sub(1));
    let mut used = 0;
    let mut prefix = Vec::new();
    for symbol in &symbols[..prefix_end] {
        let symbol_width = symbol.width();
        if used + symbol_width > maximum {
            break;
        }
        used += symbol_width;
        prefix.push(StyledSymbol {
            symbol: if symbol.is_whitespace() {
                symbol.symbol.clone()
            } else {
                " ".repeat(symbol_width)
            },
            style: symbol.style,
            offset: None,
        });
    }
    prefix
}

fn structural_marker_end(symbols: &[StyledSymbol], start: usize) -> Option<usize> {
    let symbol = |index: usize| symbols.get(index).map(|symbol| symbol.symbol.as_str());
    if matches!(symbol(start), Some("•" | "│" | "✓" | "×" | "⠋"))
        && symbols
            .get(start + 1)
            .is_some_and(StyledSymbol::is_whitespace)
    {
        return Some(start + 2);
    }
    if symbol(start) == Some("[")
        && matches!(symbol(start + 1), Some(" " | "x" | "X"))
        && symbol(start + 2) == Some("]")
        && symbols
            .get(start + 3)
            .is_some_and(StyledSymbol::is_whitespace)
    {
        return Some(start + 4);
    }

    let digits_end = symbols[start..]
        .iter()
        .take_while(|symbol| {
            symbol.symbol.len() == 1
                && symbol
                    .symbol
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
        })
        .count()
        + start;
    (digits_end > start
        && matches!(symbol(digits_end), Some("." | ")"))
        && symbols
            .get(digits_end + 1)
            .is_some_and(StyledSymbol::is_whitespace))
    .then_some(digits_end + 2)
}

/// The offset of the first symbol still waiting to be placed on a row.
fn next_offset(
    pending_whitespace: &VecDeque<StyledSymbol>,
    pending_word: &[StyledSymbol],
) -> Option<usize> {
    pending_whitespace
        .front()
        .or(pending_word.first())
        .and_then(|next| next.offset)
}

/// A row from the symbols it draws. `fallback` is where the row's text would
/// begin had it any: the offset of the next symbol still to be placed.
fn row_from_symbols(symbols: Vec<StyledSymbol>, fallback: usize) -> StyledRow {
    let indent = symbols
        .iter()
        .take_while(|symbol| symbol.offset.is_none())
        .map(StyledSymbol::width)
        .sum();
    let mut placed = symbols.iter().filter_map(|symbol| {
        symbol
            .offset
            .map(|offset| (offset, offset + symbol.symbol.len()))
    });
    let (start, end) = match placed.next() {
        Some((start, first_end)) => (start, placed.next_back().map_or(first_end, |(_, end)| end)),
        None => (fallback, fallback),
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    for symbol in symbols {
        if let Some(span) = spans.last_mut()
            && span.style == symbol.style
        {
            span.content.to_mut().push_str(&symbol.symbol);
        } else {
            spans.push(Span::styled(symbol.symbol, symbol.style));
        }
    }
    StyledRow {
        start,
        end,
        indent,
        line: Line::from(spans),
    }
}

/// Ratatui's untrimmed word-wrapper with one deliberate extension: once the
/// first row fills, every later row starts with `prefix`. Keeping the wrapper
/// here makes cached rows, viewport slicing, and the cells a pointer resolves
/// agree, since all three come from this one wrap.
fn wrap_with_continuation_indent(
    symbols: Vec<StyledSymbol>,
    text_len: usize,
    width: u16,
    prefix: &[StyledSymbol],
) -> Vec<StyledRow> {
    let prefix_width = prefix.iter().map(StyledSymbol::width).sum::<usize>();
    let maximum = usize::from(width);
    let mut wrapped: Vec<StyledRow> = Vec::new();
    let mut pending_line: Vec<StyledSymbol> = Vec::new();
    let mut pending_word: Vec<StyledSymbol> = Vec::new();
    let mut pending_whitespace: VecDeque<StyledSymbol> = VecDeque::new();
    let mut line_width = 0usize;
    let mut word_width = 0usize;
    let mut whitespace_width = 0usize;
    let mut non_whitespace_previous = false;

    for symbol in symbols {
        let is_whitespace = symbol.is_whitespace();
        let symbol_width = symbol.width();
        if symbol_width > maximum {
            continue;
        }

        let word_found = non_whitespace_previous && is_whitespace;
        let current_prefix_symbols = if wrapped.is_empty() { 0 } else { prefix.len() };
        let untrimmed_overflow = pending_line.len() == current_prefix_symbols
            && word_width + whitespace_width + line_width + symbol_width > maximum;
        if word_found || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width += whitespace_width;
            pending_line.append(&mut pending_word);
            line_width += word_width;
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= maximum;
        let pending_word_overflow =
            symbol_width > 0 && line_width + whitespace_width + word_width >= maximum;
        if line_full || pending_word_overflow {
            let mut remaining_width = maximum.saturating_sub(line_width);
            let fallback = next_offset(&pending_whitespace, &pending_word)
                .or(symbol.offset)
                .unwrap_or(text_len);
            wrapped.push(row_from_symbols(
                std::mem::take(&mut pending_line),
                fallback,
            ));
            pending_line.extend(prefix.iter().cloned());
            line_width = prefix_width;

            while let Some(whitespace) = pending_whitespace.front() {
                let whitespace_symbol_width = whitespace.width();
                if whitespace_symbol_width > remaining_width {
                    break;
                }
                whitespace_width -= whitespace_symbol_width;
                remaining_width -= whitespace_symbol_width;
                pending_whitespace.pop_front();
            }
            if is_whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        if is_whitespace {
            whitespace_width += symbol_width;
            pending_whitespace.push_back(symbol);
        } else {
            word_width += symbol_width;
            pending_word.push(symbol);
        }
        non_whitespace_previous = !is_whitespace;
    }

    let fallback = next_offset(&pending_whitespace, &pending_word).unwrap_or(text_len);
    if pending_line.is_empty() && pending_word.is_empty() && !pending_whitespace.is_empty() {
        wrapped.push(row_from_symbols(Vec::new(), fallback));
    }
    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() && (wrapped.is_empty() || pending_line.len() != prefix.len()) {
        wrapped.push(row_from_symbols(pending_line, fallback));
    }
    if wrapped.is_empty() {
        wrapped.push(row_from_symbols(Vec::new(), text_len));
    }
    wrapped
}

/// The run of non-whitespace characters starting at `offset`.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let end = text[offset..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| offset + index)
        .unwrap_or(text.len());
    offset..end
}

fn display_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::{Color, Style},
        text::Line,
        widgets::{Paragraph, Wrap},
    };

    use super::{StyledLayout, StyledLine, StyledSpan, TextLayout};

    fn styled(text: &str) -> StyledLine {
        StyledLine::text(text, Style::default())
    }

    fn drawn_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| &*span.content).collect()
    }

    fn paragraph_rows(line: &Line<'static>, width: u16) -> Vec<String> {
        let rows = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            width,
            u16::try_from(rows).expect("row count fits a terminal"),
        ))
        .expect("create test terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(line.clone()).wrap(Wrap { trim: false }),
                    frame.area(),
                )
            })
            .expect("render paragraph");
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn laid_out_rows(line: &StyledLine, width: u16) -> Vec<String> {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            width,
            u16::try_from(StyledLayout::new(line, width).row_count()).expect("rows fit"),
        ))
        .expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = StyledLayout::new(line, width);
                let area = frame.area();
                for (index, row) in layout.rows().iter().enumerate() {
                    super::draw_row(
                        frame.buffer_mut(),
                        area.x,
                        area.y + index as u16,
                        area.width,
                        &row.line,
                    );
                }
            })
            .expect("draw rows");
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Lines without leading whitespace or a Markdown marker take no hanging
    /// indent, so their rows are exactly ratatui's; the corpus stays inside
    /// that so the comparison is against what the Transcript drew before.
    #[test]
    fn styled_rows_draw_as_ratatui_paragraph_wrapping_drew_them() {
        let corpus = vec![
            styled(""),
            styled("x"),
            styled("word"),
            styled("exactly-tw"),
            styled("just-over-w"),
            styled(&"\u{5b57}".repeat(6)),
            styled(&"word ".repeat(40)),
            StyledLine::from(vec![
                StyledSpan::text("styled ", Style::default()),
                StyledSpan::text("span pieces", Style::default().fg(Color::Rgb(9, 8, 7))),
            ]),
            styled("a \u{5b57}\u{5b57} mixed width content line"),
            styled("aaaaaaaaaaaa bbb"),
            styled("hello   wonderful world with    runs of spaces"),
            styled("supercalifragilisticexpialidocious and more"),
            styled("ab \u{1f642}\u{1f642} wide \u{1f642}"),
            styled("trailing whitespace here      "),
        ];
        for width in [1u16, 2, 5, 10, 11, 26, 80] {
            for line in &corpus {
                assert_eq!(
                    laid_out_rows(line, width),
                    paragraph_rows(&line.to_line(), width),
                    "rows diverged for {:?} at width {width}",
                    line.written_text()
                );
            }
        }
    }

    #[test]
    fn a_continuation_row_hangs_beneath_a_list_items_text() {
        let line = styled("• first item that wraps onto another row");
        let layout = StyledLayout::new(&line, 20);
        let rows = layout.rows();
        assert_eq!(
            rows.iter()
                .map(|row| drawn_text(&row.line))
                .collect::<Vec<_>>(),
            vec!["• first item that", "  wraps onto another", "  row"]
        );
        assert_eq!(
            (rows[1].indent, rows[1].start),
            (2, "• first item that ".len())
        );
        assert_eq!(rows[1].end, "• first item that wraps onto another".len());
        assert_eq!(
            rows[2].start,
            "• first item that wraps onto another ".len(),
            "the space the wrap dropped lies between one row's end and the next's start"
        );
    }

    #[test]
    fn a_row_answers_the_byte_offset_behind_each_of_its_columns() {
        let line = styled("hello wonderful \u{5b57}\u{5b57} world");
        let layout = StyledLayout::new(&line, 12);
        let rows = layout.rows();
        assert_eq!(
            rows.iter()
                .map(|row| drawn_text(&row.line))
                .collect::<Vec<_>>(),
            vec!["hello", "wonderful", "\u{5b57}\u{5b57} world"]
        );
        assert_eq!(rows[0].offset_at(&line, 12, 0), 0);
        assert_eq!(rows[0].offset_at(&line, 12, 4), 4);
        assert_eq!(
            rows[0].offset_at(&line, 12, 11),
            "hello".len(),
            "a column past the row's text answers the offset past its last character"
        );
        assert_eq!(rows[1].offset_at(&line, 12, 3), "hello won".len());
        let first_wide = "hello wonderful ".len();
        assert_eq!(rows[2].offset_at(&line, 12, 0), first_wide);
        assert_eq!(
            rows[2].offset_at(&line, 12, 1),
            first_wide,
            "both cells of a wide character answer the character's offset"
        );
        assert_eq!(
            rows[2].offset_at(&line, 12, 2),
            first_wide + "\u{5b57}".len()
        );
    }

    #[test]
    fn a_column_in_the_hanging_indent_answers_where_the_row_begins() {
        let line = styled("• first item that wraps onto another row");
        let layout = StyledLayout::new(&line, 20);
        let row = &layout.rows()[1];
        assert_eq!(row.offset_at(&line, 20, 0), row.start);
        assert_eq!(row.offset_at(&line, 20, 1), row.start);
        assert_eq!(row.offset_at(&line, 20, 2), row.start);
        assert_eq!(row.offset_at(&line, 20, 3), row.start + 1);
    }

    #[test]
    fn a_fitting_line_is_one_row_with_its_spans_intact() {
        let line = StyledLine::from(vec![
            StyledSpan::chrome("  ", Style::default()),
            StyledSpan::text("plain", Style::default().fg(Color::Red)),
        ]);
        let layout = StyledLayout::new(&line, 40);
        assert_eq!(layout.row_count(), 1);
        let row = &layout.rows()[0];
        assert_eq!((row.start, row.end, row.indent), (0, "  plain".len(), 0));
        assert_eq!(row.line, line.to_line());
    }

    fn rows(text: &str, width: u16) -> Vec<String> {
        TextLayout::new(text, width)
            .rows()
            .map(|row| row.text.to_owned())
            .collect()
    }

    #[test]
    fn a_word_that_does_not_fit_the_room_left_moves_to_the_next_row_whole() {
        assert_eq!(
            rows("hello wonderful world", 12),
            vec!["hello ", "wonderful ", "world"],
            "a word moves entire rather than leaving its first characters behind"
        );
    }

    #[test]
    fn a_word_too_wide_for_a_row_of_its_own_is_split_within() {
        assert_eq!(
            rows("hi abcdefghijkl", 6),
            vec!["hi ", "abcdef", "ghijkl"],
            "a word with nowhere to move to fills rows character by character"
        );
        assert_eq!(
            rows("abcdefghij", 4),
            vec!["abcd", "efgh", "ij"],
            "a lone oversized word starts on the first row it is already on"
        );
    }

    #[test]
    fn newlines_break_rows_and_a_trailing_newline_opens_an_empty_row() {
        assert_eq!(rows("one\ntwo", 12), vec!["one", "two"]);
        assert_eq!(rows("one\n", 12), vec!["one", ""]);
        assert_eq!(rows("", 12), vec![""]);
    }

    #[test]
    fn a_caret_at_a_wrapped_word_reads_from_the_row_the_word_moved_to() {
        let layout = TextLayout::new("hello wonderful world", 12);
        assert_eq!(
            layout.cursor_position(6),
            (1, 0),
            "the caret before a moved word sits where that word is drawn"
        );
        assert_eq!(layout.cursor_position(5), (0, 5), "the space stays behind");
        assert_eq!(layout.cursor_position(9), (1, 3));
        assert_eq!(layout.row_count(), 3);
    }

    #[test]
    fn a_caret_at_a_full_rows_right_edge_spills_to_the_row_below() {
        let filled = "x".repeat(12);
        let layout = TextLayout::new(&filled, 12);
        assert_eq!(layout.row_count(), 1, "a filled row is one row of text");
        assert_eq!(
            layout.cursor_position(12),
            (1, 0),
            "the next character typed goes to the row below"
        );
    }

    #[test]
    fn the_space_that_ends_a_row_stays_with_it_rather_than_indenting_the_next() {
        assert_eq!(
            rows("aaaaaaaaaaaa bbb", 12),
            vec!["aaaaaaaaaaaa", "bbb"],
            "the moved word starts at the left edge, the space it left behind \
             showing nowhere"
        );
        assert_eq!(
            TextLayout::new("aaaa        ", 8).row_count(),
            1,
            "trailing spaces do not grow the composer by a row of blanks"
        );
    }

    #[test]
    fn a_caret_at_the_edge_of_a_line_a_newline_ended_waits_in_that_lines_margin() {
        let layout = TextLayout::new("aaaa\nbb", 4);
        assert_eq!(
            layout.cursor_position(4),
            (0, 4),
            "the end of a full line keeps its own row rather than the next line's"
        );
        assert_eq!(
            layout.cursor_position(5),
            (1, 0),
            "the next line starts fresh"
        );
    }

    #[test]
    fn a_character_too_wide_for_a_row_of_its_own_is_kept_rather_than_dropped() {
        let layout = TextLayout::new("🙂", 1);
        let row = layout
            .rows()
            .next()
            .expect("the text lays out over one row");
        assert_eq!(
            (row.text, row.width),
            ("🙂", 2),
            "a character with nowhere narrower to go still shows"
        );
    }

    #[test]
    fn wide_characters_are_measured_by_the_columns_they_occupy() {
        assert_eq!(rows("ab 🙂🙂", 5), vec!["ab ", "🙂🙂"]);
        let layout = TextLayout::new("🙂🙂", 3);
        assert_eq!(
            layout.cursor_position("🙂".len()),
            (1, 0),
            "a wide character that cannot finish a row starts the next one"
        );
    }
}
