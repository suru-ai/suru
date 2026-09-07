//! Selection coordinates belong to the surface that painted the text.
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SelectionCell {
    pub(super) row: usize,
    pub(super) column: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TextSelection {
    pub(super) surface: SelectionSurface,
    pub(super) anchor: SelectionCell,
    pub(super) focus: SelectionCell,
    pub(super) epoch: u64,
    /// The unit the press that made the selection marked, so a later drag
    /// can grow it by the same unit.
    pub(super) granularity: SelectionGranularity,
}

impl TextSelection {
    pub(super) fn ordered(self) -> (SelectionCell, SelectionCell) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }
}

/// What one press marks: a cell to drag from, a word, or a whole Line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionGranularity {
    Cell,
    Word,
    Line,
}

/// The class a grapheme belongs to for word selection, decided by its first
/// scalar value. Consecutive graphemes of one class form a word, so a run of
/// spaces or of punctuation is a word of its own, and CJK text is a word
/// apart from Latin text beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WordClass {
    Whitespace,
    Delimiter,
    Cjk,
    Text,
}

impl WordClass {
    /// Ghostty's delimiter set: slash, dot, and hyphen stay inside a word so
    /// a path, a dotted name, or a kebab-case flag selects whole.
    const DELIMITERS: &'static str = "'\"`\u{2502}|:;,()[]{}<>$";

    fn of(grapheme: &str) -> Self {
        let Some(first) = grapheme.chars().next() else {
            return Self::Text;
        };
        if first == ' ' || first == '\t' {
            Self::Whitespace
        } else if Self::DELIMITERS.contains(first) {
            Self::Delimiter
        } else if is_cjk(first) {
            Self::Cjk
        } else {
            Self::Text
        }
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        u32::from(character),
        0x1100..=0x11FF       // Hangul Jamo
        | 0x2E80..=0x2FDF     // CJK and Kangxi radicals
        | 0x3000..=0x303F     // CJK symbols and punctuation
        | 0x3040..=0x30FF     // Hiragana and Katakana
        | 0x3100..=0x31FF     // Bopomofo, Hangul compatibility Jamo, Kanbun
        | 0x3400..=0x4DBF     // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0xA960..=0xA97F     // Hangul Jamo Extended-A
        | 0xAC00..=0xD7FF     // Hangul syllables and Jamo Extended-B
        | 0xF900..=0xFAFF     // CJK Compatibility Ideographs
        | 0xFF00..=0xFFEF     // Halfwidth and fullwidth forms
        | 0x20000..=0x3134F   // CJK Unified Ideographs Extensions B onward
    )
}

/// The byte range of the word in `text` around `offset`: the run of
/// graphemes sharing the class of the grapheme at `offset`, and for a URL the
/// scheme and the path either side of its `://`, since the colon that would
/// otherwise split them is what makes the token a link. `None` when `offset`
/// is past the text.
pub(super) fn word_range(text: &str, offset: usize) -> Option<std::ops::Range<usize>> {
    use unicode_segmentation::UnicodeSegmentation;
    if offset >= text.len() {
        return None;
    }
    let graphemes: Vec<(usize, WordClass)> = text
        .grapheme_indices(true)
        .map(|(start, grapheme)| (start, WordClass::of(grapheme)))
        .collect();
    let run_at = |offset: usize| {
        let index = graphemes.iter().rposition(|(start, _)| *start <= offset)?;
        let class = graphemes[index].1;
        let first = graphemes[..index]
            .iter()
            .rposition(|(_, other)| *other != class)
            .map_or(0, |before| before + 1);
        let end = graphemes[index + 1..]
            .iter()
            .find(|(_, other)| *other != class)
            .map_or(text.len(), |(start, _)| *start);
        Some((class, graphemes[first].0..end))
    };
    // A URL is a scheme run, the colon (a delimiter run of its own), and a
    // path run beginning with two slashes.
    let scheme_before = |colon: usize| {
        let (class, scheme) = run_at(colon.checked_sub(1)?)?;
        (class == WordClass::Text).then_some(scheme)
    };
    let path_after = |colon: usize| {
        let (class, path) = run_at(colon + 1)?;
        (class == WordClass::Text && text[path.clone()].starts_with("//")).then_some(path)
    };
    let (class, range) = run_at(offset)?;
    match class {
        WordClass::Delimiter if &text[range.clone()] == ":" => {
            match (scheme_before(range.start), path_after(range.start)) {
                (Some(scheme), Some(path)) => Some(scheme.start..path.end),
                _ => Some(range),
            }
        }
        WordClass::Text => {
            let mut range = range;
            if text[range.end..].starts_with(':')
                && let Some(path) = path_after(range.end)
            {
                range.end = path.end;
            }
            if text[range.clone()].starts_with("//")
                && text[..range.start].ends_with(':')
                && let Some(scheme) = scheme_before(range.start - 1)
            {
                range.start = scheme.start;
            }
            Some(range)
        }
        _ => Some(range),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionSurface {
    Transcript,
    Composer,
    Sidebar,
    Settings,
    NumericEditor,
    Serve,
    Connect,
    Sessions,
    Workspaces,
    Models,
    Themes,
    ModelOptions,
    Completions,
    Subagents,
    SidebarMenu,
}

/// A prose surface recorded by the draw, before selection highlighting.
#[derive(Clone, Debug)]
pub(super) struct SelectionFrame {
    pub(super) surface: SelectionSurface,
    pub(super) area: Rect,
    pub(super) scroll: usize,
    pub(super) text: String,
    pub(super) rows: Vec<std::ops::Range<usize>>,
}

impl SelectionFrame {
    /// Painted content must still be the content that received the press.
    pub(super) fn epoch(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.area.hash(&mut hash);
        self.text.hash(&mut hash);
        hash.finish()
    }

    /// The draft bytes beneath a cell, including the whole glyph it paints.
    pub(super) fn cell_range(&self, cell: SelectionCell) -> Option<std::ops::Range<usize>> {
        let range = self.rows.get(cell.row)?;
        let mut column = 0;
        for (offset, glyph) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(
            &self.text[range.clone()],
            true,
        ) {
            column += glyph.width();
            if cell.column < column {
                let start = range.start + offset;
                return Some(start..start + glyph.len());
            }
        }
        Some(range.end..range.end)
    }

    pub(super) fn highlight_range(&self, selected: std::ops::Range<usize>, buffer: &mut Buffer) {
        for y in self.area.y..self.area.bottom() {
            let Some(row) = self.rows.get(self.scroll + usize::from(y - self.area.y)) else {
                break;
            };
            let mut column = 0;
            let text = &self.text[row.clone()];
            // Composer rows map exactly to draft text: trailing spaces are editable
            // characters too. Other surfaces may carry layout padding.
            let text = if self.surface == SelectionSurface::Composer {
                text
            } else {
                text.trim_end()
            };
            for (offset, glyph) in
                unicode_segmentation::UnicodeSegmentation::grapheme_indices(text, true)
            {
                let left = column;
                column += glyph.width();
                let start = row.start + offset;
                if start < selected.end && start + glyph.len() > selected.start {
                    for x in left..column.min(usize::from(self.area.width)) {
                        buffer[(self.area.x + x as u16, y)]
                            .modifier
                            .insert(Modifier::REVERSED);
                    }
                }
            }
        }
    }

    pub(super) fn cell(&self, position: ratatui::layout::Position) -> Option<SelectionCell> {
        if self.area.is_empty() || self.rows.is_empty() {
            return None;
        }
        Some(SelectionCell {
            row: (self.scroll
                + usize::from(position.y.clamp(self.area.y, self.area.bottom() - 1) - self.area.y))
            .min(self.rows.len() - 1),
            column: usize::from(position.x.clamp(self.area.x, self.area.right() - 1) - self.area.x),
        })
    }

    /// Reverses the cells a selection covers, the way [`Self::copy`] reads
    /// them: the text each row holds, up to its last non-blank glyph. Padding
    /// past the text and rows holding no text stay as drawn, so what lights
    /// up is what a copy would hold.
    pub(super) fn highlight(&self, selection: TextSelection, buffer: &mut Buffer) {
        let (start, end) = selection.ordered();
        let area = self.area;
        for y in area.y..area.bottom() {
            let row = self.scroll + usize::from(y - area.y);
            if row < start.row {
                continue;
            }
            if row > end.row {
                break;
            }
            let Some(range) = self.rows.get(row) else {
                break;
            };
            let mut column = 0;
            for glyph in unicode_segmentation::UnicodeSegmentation::graphemes(
                self.text[range.clone()].trim_end(),
                true,
            ) {
                let width = glyph.width();
                let left = column;
                column += width;
                if width == 0 {
                    continue;
                }
                let selected_from_start = row > start.row || column > start.column;
                let selected_to_end = row < end.row || left <= end.column;
                if !(selected_from_start && selected_to_end) {
                    continue;
                }
                let left = u16::try_from(left).unwrap_or(u16::MAX);
                let right = u16::try_from(column).unwrap_or(u16::MAX);
                for x in area.x.saturating_add(left)..area.x.saturating_add(right).min(area.right())
                {
                    buffer[(x, y)].modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    pub(super) fn copy(&self, selection: TextSelection) -> Option<String> {
        let (start, end) = selection.ordered();
        let start = self.cell_range(start)?.start;
        let end = self.cell_range(end)?.end;
        Some(copy_text(self.text.get(start..end)?))
    }
}

/// Clipboard prose omits trailing whitespace while preserving written Line breaks.
/// Cutting draft text bypasses this normalization so every removed byte is copied.
pub(super) fn copy_text(text: &str) -> String {
    text.split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

impl SelectionSurface {
    pub(super) fn is_overlay(self) -> bool {
        !matches!(self, Self::Transcript | Self::Composer | Self::Sidebar)
    }
}

impl SelectionFrame {
    pub(super) fn painted(surface: SelectionSurface, buffer: &Buffer, area: Rect) -> Self {
        let mut text = String::new();
        let mut rows = Vec::new();
        for y in area.y..area.bottom() {
            if y > area.y {
                text.push('\n');
            }
            let start = text.len();
            let mut x = area.x;
            while x < area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x = x.saturating_add(u16::try_from(symbol.width()).unwrap_or(1).max(1));
            }
            rows.push(start..text.len());
        }
        Self {
            surface,
            area,
            scroll: 0,
            text,
            rows,
        }
    }
}
