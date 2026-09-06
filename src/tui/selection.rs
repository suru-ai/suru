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
}

impl TextSelection {
    pub(super) fn ordered(self) -> (SelectionCell, SelectionCell) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
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
    /// Composer edits are copy-only: they keep the standing selection.
    pub(super) fn epoch(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        if self.surface == SelectionSurface::Composer {
            self.area.width.hash(&mut hash);
        } else {
            self.area.hash(&mut hash);
            self.text.hash(&mut hash);
        }
        hash.finish()
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
        let offset = |cell: SelectionCell, inclusive: bool| {
            let range = self.rows.get(cell.row)?;
            let mut column = 0;
            for (byte, glyph) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(
                &self.text[range.clone()],
                true,
            ) {
                column += glyph.width();
                if cell.column < column {
                    return Some(range.start + byte + if inclusive { glyph.len() } else { 0 });
                }
            }
            Some(range.end)
        };
        let start = offset(start, false)?;
        let end = offset(end, true)?;
        Some(
            self.text
                .get(start..end)?
                .split('\n')
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
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
