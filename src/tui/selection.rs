//! A Transcript selection stays in content coordinates across scrolling.
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct TranscriptCell {
    pub(super) row: usize,
    pub(super) column: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TextSelection {
    pub(super) anchor: TranscriptCell,
    pub(super) focus: TranscriptCell,
    pub(super) epoch: u64,
}

impl TextSelection {
    pub(super) fn ordered(self) -> (TranscriptCell, TranscriptCell) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    pub(super) fn highlight(self, buffer: &mut Buffer, area: Rect, scroll: usize) {
        let (start, end) = self.ordered();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = TranscriptCell {
                    row: scroll + usize::from(y - area.y),
                    column: usize::from(x - area.x),
                };
                let width = u16::try_from(buffer[(x, y)].symbol().width())
                    .unwrap_or(1)
                    .max(1);
                let right = x.saturating_add(width).min(area.right());
                let last = TranscriptCell {
                    column: cell.column + usize::from(right - x - 1),
                    ..cell
                };
                if last >= start && cell <= end {
                    for column in x..right {
                        buffer[(column, y)].modifier.insert(Modifier::REVERSED);
                    }
                }
            }
        }
    }
}
