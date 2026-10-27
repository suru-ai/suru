//! What a details pane drew: the rows it filled, and each line it wrapped over
//! rows of its own, so a selection reads the line back as the one line it was
//! written as (ADR-0020). The Model picker and the settings panel each detail
//! what focus stands on in a pane beside their list, and both lay it out here.

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
        if self.lines.len() <= height {
            return;
        }
        let kept = height.saturating_sub(1);
        self.lines.truncate(kept);
        if height > 0 {
            self.lines.push(Line::styled("…", theme.text.subdued));
        }
        self.wrapped.retain_mut(|line| {
            let rows = kept.saturating_sub(line.row);
            line.parts.truncate(rows);
            !line.parts.is_empty()
        });
    }
}
