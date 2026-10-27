//! How the settings panel draws a list of its rows, wherever it draws one: a
//! tab's rows, a Provider's own Settings beneath the line a narrow panel heads
//! them with, and those Settings as the pane beside a wide panel lists them.
//! Every row ends on one column saying what its Setting is worth — or, for a
//! Provider, what condition it is in — and a mark for a value the reader
//! pinned, so a list reads the same whichever surface draws it.

use std::{borrow::Cow, ops::Range};

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    theme::Theme,
    tui::{
        list_window::ListWindow, provider_icons::provider_label, slots::truncate_to_width, spinner,
    },
};

use super::{RowAvailability, RowValue, RowWindow, SettingRow};

/// What a row leads with while the keys are on it, and what every other row
/// stands in by so the names line up.
const FOCUS: &str = "› ";
const UNFOCUSED: &str = "  ";

/// What a narrow panel heads a Provider's own Settings with, beside the
/// Provider's name, where they stand in place of the Providers: the way back
/// to them.
const BACK: &str = "‹ ";

/// What a row ends on for a value the reader pinned, where a value riding its
/// built-in default leaves the column blank.
const PINNED: &str = "●";

/// A list of the panel's rows, and how one surface draws it.
pub(in crate::tui) struct RowList<'a> {
    rows: &'a [SettingRow],
    /// The Provider whose own Settings `rows` are, named above them with the
    /// way back to the Providers. It is measured with them, so its condition
    /// stands in their value column, and is the first thing a surface too
    /// short for it and a row gives up.
    head: Option<&'a SettingRow>,
    /// Whether the keys are on this list, which draws its selected row as
    /// theirs, and otherwise dimly as the row they come back to.
    holds_keys: bool,
    show_icons: bool,
    spinner_frame: usize,
}

/// The rows one frame drew of a list, which of the list's rows the first of
/// them is, and whether a line heading them was drawn above them.
pub(in crate::tui) struct DrawnRows {
    pub(in crate::tui) lines: Vec<Line<'static>>,
    first: usize,
    head: bool,
}

impl<'a> RowList<'a> {
    pub(in crate::tui) fn new(rows: &'a [SettingRow], holds_keys: bool) -> Self {
        Self {
            rows,
            head: None,
            holds_keys,
            show_icons: false,
            spinner_frame: 0,
        }
    }

    /// The list headed by the Provider whose own Settings it is.
    pub(in crate::tui) fn headed_by(self, head: &'a SettingRow) -> Self {
        Self {
            head: Some(head),
            ..self
        }
    }

    /// The list with a Provider's icon before its name where `show_icons`
    /// says, and a Provider being read drawing its Spinner at `spinner_frame`.
    pub(in crate::tui) fn presented(self, show_icons: bool, spinner_frame: usize) -> Self {
        Self {
            show_icons,
            spinner_frame,
            ..self
        }
    }

    /// As many of the rows as `capacity` holds, each `width` columns wide,
    /// from wherever `window` settles over them — or all of them, where the
    /// surface keeps no window — beneath the line heading them where there is
    /// room for both.
    pub(in crate::tui) fn draw(
        &self,
        window: Option<&ListWindow>,
        width: usize,
        capacity: usize,
        theme: &Theme,
    ) -> DrawnRows {
        let head = self.head.filter(|_| capacity >= 2);
        let capacity = capacity.saturating_sub(usize::from(head.is_some()));
        let selected = self.rows.iter().position(|row| row.selected);
        let shown = window.map_or(0..self.rows.len(), |window| {
            window.settle_rows(self.rows.len(), capacity, selected)
        });
        // Every row the list holds is named and valued before the window
        // narrows them, so the value column measures them all and holds still
        // as the window scrolls.
        let named = self
            .rows
            .iter()
            .map(|row| {
                let marker = if row.selected && self.holds_keys {
                    FOCUS
                } else {
                    UNFOCUSED
                };
                format!("{marker}{}", self.label(row))
            })
            .collect::<Vec<_>>();
        let values = self
            .rows
            .iter()
            .map(|row| self.value(row))
            .collect::<Vec<_>>();
        let head = head.map(|row| (row, format!("{BACK}{}", self.label(row)), self.value(row)));
        let column = ValueColumn::of(
            named
                .iter()
                .chain(head.as_ref().map(|(_, name, _)| name))
                .map(|name| name.width())
                .max(),
            values
                .iter()
                .chain(head.as_ref().map(|(_, _, value)| value))
                .map(|value| value.width())
                .max(),
            width,
        );
        let mut lines = Vec::with_capacity(shown.len() + 1);
        if let Some((row, name, value)) = &head {
            lines.push(column.line(
                name,
                ValueCell::of(row, value, tone(row, theme)),
                width,
                theme.text.primary.add_modifier(Modifier::BOLD),
            ));
        }
        lines.extend(
            self.rows
                .iter()
                .zip(named.iter().zip(&values))
                .skip(shown.start)
                .take(shown.len())
                .map(|(row, (name, value))| {
                    let style = match (row.selected, self.holds_keys, &row.value) {
                        (true, true, _) => theme.selection.focused,
                        (true, false, _) => theme.selection.unfocused,
                        (false, _, RowValue::ProviderDisabled) => theme.text.subdued,
                        (false, _, _) => theme.text.primary,
                    };
                    // A condition asking something of the reader takes its
                    // own colour, except on the selected row, which keeps one
                    // block.
                    let tone = tone(row, theme).filter(|_| !row.selected);
                    column.line(name, ValueCell::of(row, value, tone), width, style)
                }),
        );
        DrawnRows {
            lines,
            first: shown.start,
            head: head.is_some(),
        }
    }

    /// What a row is called: its Setting's label, or the Provider it stands
    /// for, with that Provider's icon where the reader has icons on.
    fn label(&self, row: &SettingRow) -> String {
        row.provider.map_or_else(
            || row.label.to_owned(),
            |provider| provider_label(self.show_icons, provider, row.label),
        )
    }

    /// What a row says in the column at its end. A Setting's row says what it
    /// is worth; a Provider's says what condition it is in, which is a word
    /// at most — that the reader turned it off, a Spinner while its
    /// Availability is read, what that read found wrong once it lands — and
    /// nothing at all from a Provider still serving its catalog, because the
    /// quiet state is the good one.
    fn value<'r>(&self, row: &'r SettingRow) -> Cow<'r, str> {
        match (&row.value, &row.availability) {
            (RowValue::Choice(value), _) => Cow::Borrowed(value),
            (RowValue::ProviderDisabled, _) => Cow::Borrowed("disabled"),
            (RowValue::ProviderEnabled, RowAvailability::Quiet) => Cow::Borrowed(""),
            (RowValue::ProviderEnabled, RowAvailability::Reading) => {
                Cow::Owned(spinner::frame(self.spinner_frame / 3).to_owned())
            }
            (RowValue::ProviderEnabled, RowAvailability::Warning) => Cow::Borrowed("warning"),
            (RowValue::ProviderEnabled, RowAvailability::Unavailable { reason, .. }) => {
                Cow::Borrowed(reason.label())
            }
            (RowValue::ProviderEnabled, RowAvailability::Failed) => Cow::Borrowed("error"),
        }
    }
}

impl DrawnRows {
    /// The lines drawn for the list's rows, past any line heading them.
    fn rows(&self) -> usize {
        self.lines.len() - usize::from(self.head)
    }

    /// Where the rows stand once drawn from `top` across `columns`, for a
    /// pointer to resolve against, and nothing for a frame that drew none.
    pub(in crate::tui) fn window(&self, top: u16, columns: Range<u16>) -> Option<RowWindow> {
        (self.rows() > 0).then(|| RowWindow {
            top: top.saturating_add(u16::from(self.head)),
            first: self.first,
            count: u16::try_from(self.rows()).unwrap_or(u16::MAX),
            columns,
        })
    }

    /// Where the line heading the rows stands once drawn from `top`, as the
    /// row `stands_for` of the list it stands for, and nothing for a frame
    /// that drew no such line.
    pub(in crate::tui) fn head_window(
        &self,
        top: u16,
        columns: Range<u16>,
        stands_for: usize,
    ) -> Option<RowWindow> {
        self.head.then_some(RowWindow {
            top,
            first: stands_for,
            count: 1,
            columns,
        })
    }
}

/// The colour a Provider's condition takes on its row where it asks something
/// of the reader: a failed read is Suru's error, and anything they fix outside
/// Suru is a warning.
fn tone(row: &SettingRow, theme: &Theme) -> Option<Style> {
    match row.availability {
        RowAvailability::Warning | RowAvailability::Unavailable { .. } => {
            Some(theme.feedback.warning)
        }
        RowAvailability::Failed => Some(theme.feedback.error),
        RowAvailability::Quiet | RowAvailability::Reading => None,
    }
}

/// What a row ends on: what its Setting is worth or its Provider's condition,
/// in a colour of its own where it asks something of the reader, and whether
/// the reader pinned it.
struct ValueCell<'v> {
    text: &'v str,
    tone: Option<Style>,
    pinned: bool,
}

impl<'v> ValueCell<'v> {
    fn of(row: &SettingRow, text: &'v str, tone: Option<Style>) -> Self {
        Self {
            text,
            tone,
            pinned: row.pinned,
        }
    }
}

/// The column at the end of a row saying what its Setting is worth, and the
/// mark after it for a value the reader pinned. It is as wide as the widest
/// value its list holds, so it holds still as the rows scroll, short of
/// crowding the names: it takes no more than the widest name leaves it, and
/// never less than a third of the row.
#[derive(Clone, Copy, Debug)]
struct ValueColumn {
    width: usize,
}

impl ValueColumn {
    /// The columns around a value: the gap before it, and the space and the
    /// pin mark after it.
    const CHROME: usize = 4;

    fn of(widest_name: Option<usize>, widest_value: Option<usize>, row_width: usize) -> Self {
        let room = row_width
            .saturating_sub(widest_name.unwrap_or(0) + Self::CHROME)
            .max(row_width / 3);
        Self {
            width: widest_value.unwrap_or(0).min(room),
        }
    }

    /// A whole row `width` columns wide in `style`: `name`, cut short before
    /// the column rather than into it, then the cell in the column.
    fn line(self, name: &str, cell: ValueCell<'_>, width: usize, style: Style) -> Line<'static> {
        let value = truncate_to_width(cell.text, self.width);
        let padding = " ".repeat(self.width.saturating_sub(value.width()));
        let pin = if cell.pinned { PINNED } else { " " };
        let cells = truncate_to_width(&format!("  {value}{padding} {pin}"), width);
        let room = width.saturating_sub(cells.width());
        let name = truncate_to_width(name, room);
        let padding = " ".repeat(room.saturating_sub(name.width()));
        Line::from(vec![
            Span::styled(format!("{name}{padding}"), style),
            Span::styled(cells, cell.tone.unwrap_or(style)),
        ])
    }
}
