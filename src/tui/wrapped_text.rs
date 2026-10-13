//! A line a surface wraps over rows of its own: broken between words so an
//! install page's address is never cut short at the edge, stood in where it
//! belongs beneath something, and read back by a selection as the one line it
//! was written as rather than as the rows it was drawn on (ADR-0020).
//!
//! What fixes an unavailable Provider is the chief of these. The Model
//! picker's retry row, the settings panel's headline, and Onboarding's
//! Provider rows each draw one, and Onboarding draws why its Providers step
//! cannot yet be passed the same way, so a line's rows and the parts of it
//! they draw come from here together and cannot disagree.

use std::ops::Range;

use ratatui::{style::Style, text::Line};
use unicode_width::UnicodeWidthStr;

use super::{selection::WrappedLine, text_layout::TextLayout};

/// How far a fix stands in beneath the row it belongs to: past the row's
/// focus marker and two columns more, so it reads as the row's own.
pub(super) const FIX_INDENT: &str = "    ";

/// A line laid out for a surface `width` columns wide.
#[derive(Clone, Debug)]
pub(super) struct WrappedText<'a> {
    text: &'a str,
    indent: &'static str,
    /// The part of `text` each row draws, top to bottom.
    parts: Vec<Range<usize>>,
}

impl<'a> WrappedText<'a> {
    /// `text` broken between words to fit `width` columns once `indent` has
    /// stood it in.
    pub(super) fn new(text: &'a str, indent: &'static str, width: u16) -> Self {
        let indented = u16::try_from(indent.width()).unwrap_or(u16::MAX);
        let parts = TextLayout::new(text, width.saturating_sub(indented))
            .rows()
            .map(|row| row.start..row.start + row.text.len())
            .collect();
        Self {
            text,
            indent,
            parts,
        }
    }

    /// How many rows the line takes.
    pub(super) fn height(&self) -> usize {
        self.parts.len()
    }

    /// The line within its last `room` rows. A fix taller than that gives up
    /// its start rather than its end, which is where every fix names the page
    /// or command it sends the reader to.
    pub(super) fn last(mut self, room: usize) -> Self {
        self.parts.drain(..self.parts.len().saturating_sub(room));
        self
    }

    /// The line within its first `room` rows, for a line whose start says
    /// the most.
    pub(super) fn first(mut self, room: usize) -> Self {
        self.parts.truncate(room);
        self
    }

    /// The rows as drawn, each in `style`.
    pub(super) fn lines(&self, style: Style) -> impl Iterator<Item = Line<'static>> + '_ {
        self.parts.iter().map(move |part| {
            Line::styled(
                format!("{}{}", self.indent, &self.text[part.clone()]),
                style,
            )
        })
    }

    /// The line as a selection reads it, drawn from `row` of the surface's
    /// content down.
    pub(super) fn wrapped(&self, row: usize) -> WrappedLine<'a> {
        WrappedLine {
            row,
            column: self.indent.width(),
            text: self.text,
            parts: self.parts.clone(),
        }
    }
}
