//! What a details pane drew: the rows it filled, and each line it wrapped over
//! rows of its own, so a selection reads the line back as the one line it was
//! written as (ADR-0020). The Model picker and the settings panel each detail
//! what focus stands on in a pane beside their list, and both lay it out here.

use std::ops::Range;

use ratatui::{style::Style, text::Line};

use crate::theme::Theme;

use super::{selection::WrappedLine, slots::truncate_to_width, wrapped_text::WrappedText};

/// The rows a pane drew, and each line it wrapped over rows of its own.
#[derive(Default)]
pub(super) struct DrawnPane<'a> {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) wrapped: Vec<WrappedLine<'a>>,
}

impl<'a> DrawnPane<'a> {
    /// `text` on one row `width` columns wide, cut short where it is wider.
    pub(super) fn line(&mut self, text: &str, width: u16, style: Style) {
        self.lines.push(Line::styled(
            truncate_to_width(text, usize::from(width)),
            style,
        ));
    }

    /// `text` broken over rows of its own `width` columns wide.
    pub(super) fn wrap(&mut self, text: &'a str, indent: &'static str, width: u16, style: Style) {
        if text.is_empty() {
            return;
        }
        let wrapped = WrappedText::new(text, indent, width);
        self.wrapped.push(wrapped.wrapped(self.lines.len()));
        self.lines.extend(wrapped.lines(style));
    }

    /// [`Self::wrap`], with `lead` standing in its first row's indent: a
    /// marker or a label the text reads on from, which a selection leaves out
    /// as it does the indent.
    pub(super) fn wrap_led(
        &mut self,
        lead: &str,
        text: &'a str,
        indent: &'static str,
        width: u16,
        style: Style,
    ) {
        let first = self.lines.len();
        self.wrap(text, indent, width, style);
        if let Some(row) = self.lines.get_mut(first) {
            let text = row.to_string();
            *row = Line::styled(format!("{lead}{}", &text[indent.len()..]), style);
        }
    }

    /// The pane cut to `height` rows, its last row an ellipsis where it had
    /// more, and each wrapped line to the rows still drawn.
    pub(super) fn fit(&mut self, height: usize, theme: &Theme) {
        self.fit_showing(height, 0, 0..0, theme);
    }

    /// The pane cut to `height` rows as [`Self::fit`] cuts it, save where
    /// that would cut into `keep`, rows the reader must see: then the rows
    /// beneath the first `heading` scroll up past an ellipsis standing for
    /// those they went by, until `keep` shows to its end, or from its start
    /// where the pane has too few rows for all of it. Answers with which rows
    /// are still drawn, and where.
    pub(super) fn fit_showing(
        &mut self,
        height: usize,
        heading: usize,
        keep: Range<usize>,
        theme: &Theme,
    ) -> PaneWindow {
        let rows = self.lines.len();
        let ellipsis = || Line::styled("…", theme.text.subdued);
        if rows <= height {
            return PaneWindow::whole(rows);
        }
        // The rows beneath the heading and the ellipsis it scrolls past.
        let room = height.saturating_sub(heading + 1);
        let first = if keep.end + 1 < rows {
            // Short of the foot, which an ellipsis then stands for too.
            (keep.end + 1).saturating_sub(room)
        } else {
            rows.saturating_sub(room)
        }
        .min(keep.start);
        if keep.is_empty() || keep.end < height || room < 2 || first <= heading {
            let kept = height.saturating_sub(1);
            self.lines.truncate(kept);
            if height > 0 {
                self.lines.push(ellipsis());
            }
            self.place_wrapped(PaneWindow::whole(kept));
            return PaneWindow::whole(kept);
        }
        let cut = rows - first > room;
        let end = if cut { first + room - 1 } else { rows };
        let window = PaneWindow {
            heading,
            first,
            end,
        };
        let shown = self.lines.drain(first..end).collect::<Vec<_>>();
        self.lines.truncate(heading);
        self.lines.push(ellipsis());
        self.lines.extend(shown);
        if cut {
            self.lines.push(ellipsis());
        }
        self.place_wrapped(window);
        window
    }

    /// Each wrapped line moved to where `window` put its rows, and cut to
    /// those it still draws.
    fn place_wrapped(&mut self, window: PaneWindow) {
        self.wrapped.retain_mut(|line| {
            let mut shown =
                (0..line.parts.len()).filter_map(|part| Some((part, window.row(line.row + part)?)));
            let Some((start, row)) = shown.next() else {
                return false;
            };
            let count = 1 + shown
                .zip(1..)
                .take_while(|((part, _), step)| *part == start + step)
                .count();
            line.row = row;
            line.parts = line.parts[start..start + count].to_vec();
            true
        });
    }
}

/// Which rows a pane drew are still drawn once it is cut to its height, and
/// where: the rows of its heading where they were, then — past an ellipsis
/// standing for any it scrolled by — the rows from `first` up to `end`.
#[derive(Clone, Copy, Debug)]
pub(super) struct PaneWindow {
    heading: usize,
    first: usize,
    end: usize,
}

impl PaneWindow {
    /// The first `end` rows, where they were drawn.
    fn whole(end: usize) -> Self {
        Self {
            heading: 0,
            first: 0,
            end,
        }
    }

    /// Where the row drawn at `row` stands now, and none for a row no longer
    /// drawn.
    pub(super) fn row(self, row: usize) -> Option<usize> {
        if row < self.heading.min(self.end) {
            return Some(row);
        }
        let ellipsis = usize::from(self.first > self.heading);
        (self.first..self.end)
            .contains(&row)
            .then(|| row - self.first + self.heading + ellipsis)
    }
}
