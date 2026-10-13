//! What fixes an unavailable Provider, as every surface that names one lays
//! it out: on lines of its own beneath what it belongs to, broken between
//! words so an install page's address is never cut short at the edge, and
//! read back by a selection as the one line it was written as (ADR-0020).
//!
//! The Model picker's retry row, the settings panel's headline, and
//! Onboarding's Provider rows all draw a fix this way, so its lines and the
//! parts of the fix they draw come from here together and cannot disagree.

use std::ops::Range;

use ratatui::{style::Style, text::Line};
use unicode_width::UnicodeWidthStr;

use super::{selection::WrappedLine, text_layout::TextLayout};

/// How far a fix stands in beneath the row it belongs to: past the row's
/// focus marker and two columns more, so it reads as the row's own.
pub(super) const FIX_INDENT: &str = "    ";

/// A fix laid out for a surface `width` columns wide.
#[derive(Clone, Debug)]
pub(super) struct FixLines<'a> {
    fix: &'a str,
    indent: &'static str,
    /// The part of `fix` each line draws, top to bottom.
    parts: Vec<Range<usize>>,
}

impl<'a> FixLines<'a> {
    /// `fix` broken between words to fit `width` columns once `indent` has
    /// stood it in.
    pub(super) fn new(fix: &'a str, indent: &'static str, width: u16) -> Self {
        let indented = u16::try_from(indent.width()).unwrap_or(u16::MAX);
        let parts = TextLayout::new(fix, width.saturating_sub(indented))
            .rows()
            .map(|row| row.start..row.start + row.text.len())
            .collect();
        Self { fix, indent, parts }
    }

    /// How many lines the fix takes.
    pub(super) fn height(&self) -> usize {
        self.parts.len()
    }

    /// The fix within `room` lines. One taller than that gives up its start
    /// rather than its end, which is where every fix names the page or
    /// command it sends the reader to.
    pub(super) fn last(mut self, room: usize) -> Self {
        self.parts.drain(..self.parts.len().saturating_sub(room));
        self
    }

    /// The lines as drawn, each in `style`.
    pub(super) fn lines(&self, style: Style) -> impl Iterator<Item = Line<'static>> + '_ {
        self.parts.iter().map(move |part| {
            Line::styled(format!("{}{}", self.indent, &self.fix[part.clone()]), style)
        })
    }

    /// The fix as a selection reads it, drawn from `row` of the surface's
    /// content down.
    pub(super) fn wrapped(&self, row: usize) -> WrappedLine<'a> {
        WrappedLine {
            row,
            column: self.indent.width(),
            text: self.fix,
            parts: self.parts.clone(),
        }
    }
}
