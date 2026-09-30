//! The Icon Picker: a Workspace-Picker-style overlay a reader opens over one
//! Session or one Workspace to choose its Icon from the Icon Catalog by hand.
//!
//! Where the Workspace Picker narrows a single column of rows, this picker
//! narrows a grid: the Catalog is around 150 to 250 glyphs, far more than a
//! column can hold, so cells stand shoulder to shoulder and searching is how
//! a reader gets to one quickly. A query always keeps focus on the grid's
//! first surviving cell rather than trying to hold a position across an
//! entirely different layout, which is what makes the picker's own arrows —
//! not a remembered index — the only thing that answers "where is the reader
//! now".
//!
//! The picker owns no listing of its own the way the Sidebar's Session
//! listing does: [`crate::icon_catalog`] is a fixed, in-memory table, so
//! `offered` reads straight from [`crate::icon_catalog::search`] every time.

use std::{cell::Cell, cell::RefCell, ops::Range};

use ratatui::layout::Position;

use crate::{
    icon_catalog::{self, IconEntry},
    protocol::{Outlook, SessionReference, WorkspaceId},
};

use super::list_window::ListWindow;

/// Splits `offered` into rows up to `columns` cells wide, starting a fresh row
/// wherever the [`crate::icon_catalog::IconGroup`] changes even if the row
/// before it ran short — the grid's own account of what a group means, so a
/// glyph standing for what the work is about never shares a row with one
/// standing for what it is written in.
fn grouped_rows(offered: &[&'static IconEntry], columns: usize) -> Vec<Vec<&'static IconEntry>> {
    let mut rows: Vec<Vec<&'static IconEntry>> = Vec::new();
    let mut current_group = None;
    for entry in offered {
        let starts_new_row = current_group != Some(entry.group)
            || rows
                .last()
                .is_some_and(|row: &Vec<&IconEntry>| row.len() >= columns);
        if starts_new_row {
            rows.push(Vec::new());
        }
        rows.last_mut()
            .expect("a row was just pushed if none was open")
            .push(entry);
        current_group = Some(entry.group);
    }
    rows
}

/// One glyph cell's width in terminal columns, not counting the gutter that
/// separates it from its neighbor. Sized to hold a Nerd Font glyph centered
/// with a column of breathing room on each side.
pub(super) const CELL_WIDTH: u16 = 3;
/// Blank columns between adjacent cells.
pub(super) const CELL_GUTTER: u16 = 1;

/// What one Icon Picker is choosing an Icon for: one Session, or one
/// Workspace — either named by Origin and identity, so a listing moving
/// behind the picker can never retarget a choice already in flight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum IconPickerTarget {
    Session(SessionReference),
    Workspace {
        origin: Outlook,
        workspace_id: WorkspaceId,
    },
}

/// One cell the grid draws: the Catalog entry it stands for, and whether it
/// is the cell the reader is on.
#[derive(Clone, Copy, Debug)]
pub(super) struct IconPickerCellView {
    pub(super) entry: &'static IconEntry,
    pub(super) selected: bool,
}

#[derive(Clone, Debug)]
struct IconPickerCellGeometry {
    row: u16,
    columns: Range<u16>,
    name: &'static str,
}

#[derive(Clone, Debug, Default)]
pub(super) struct IconPicker {
    /// What this choice would set, and whether the picker is open at all: a
    /// picker with nothing to set has nothing to be open over.
    target: Option<IconPickerTarget>,
    /// What the reader has typed to narrow the grid, read against each
    /// entry's name and keywords the way every other picker reads a query.
    query: String,
    /// The Catalog name of the cell the reader is on. Held by name rather
    /// than by grid position, so it survives the grid reflowing under a
    /// query or a resize without the picker ever having to notice either.
    selected: Option<&'static str>,
    /// The grid's column count as the frame in force laid it out, which is
    /// what turns Up and Down into a move across the flat offered list. Zero
    /// before a first frame has drawn the grid; every reader of it treats
    /// zero the same as one, so Up and Down behave like Left and Right for
    /// the one frame before real geometry is known.
    columns: Cell<usize>,
    /// Where the frame in force drew each cell, recorded at draw time and
    /// resolved against a press — the same shape [`super::subagent_picker`]
    /// records its rows in.
    geometry: RefCell<Vec<IconPickerCellGeometry>>,
    /// The window over the grid's rows, whose margin around the reader's cell
    /// is counted in rows of cells.
    window: ListWindow,
}

impl IconPicker {
    /// Opens the picker over `target`, starting from an empty query — the
    /// whole Catalog, grouped in listing order — with the first entry
    /// focused.
    pub(super) fn open(&mut self, target: IconPickerTarget) {
        self.target = Some(target);
        self.query.clear();
        self.focus_first();
        self.window.open();
    }

    pub(super) fn close(&mut self) {
        self.target = None;
        self.query.clear();
        self.selected = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.target.is_some()
    }

    pub(super) fn target(&self) -> Option<&IconPickerTarget> {
        self.target.as_ref()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    /// Takes typed text into the query and puts focus back on the grid's
    /// first surviving cell — never on wherever the reader happened to be,
    /// which a re-flowed grid may not even offer any more.
    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.focus_first();
        self.window.open();
    }

    /// Gives the last character of the query back, widening the grid again.
    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.focus_first();
        self.window.open();
    }

    pub(super) fn move_left(&mut self) {
        self.move_horizontal(-1);
    }

    pub(super) fn move_right(&mut self) {
        self.move_horizontal(1);
    }

    pub(super) fn move_up(&mut self) {
        self.move_vertical(-1);
    }

    pub(super) fn move_down(&mut self) {
        self.move_vertical(1);
    }

    /// Puts the reader on the named cell directly, which is how a pointer
    /// press chooses without walking the arrows there first. Takes the name
    /// on trust: a caller passes one straight out of [`Self::hit`], which
    /// only ever answers a name the last frame actually drew.
    pub(super) fn focus(&mut self, name: &'static str) {
        self.selected = Some(name);
    }

    /// The Catalog entry the reader is on, and `None` while a query offers
    /// nothing at all.
    pub(super) fn focused_entry(&self) -> Option<&'static IconEntry> {
        let offered = self.offered();
        self.selected_index(&offered).map(|index| offered[index])
    }

    /// The rows a grid `columns` cells wide and `capacity_rows` rows tall
    /// shows, through the window the keys carry along with the reader's
    /// cell. Also
    /// records `columns` as the layout Up and Down now answer to, the way
    /// [`Self::focused_entry`] and the movement methods expect to find it.
    pub(super) fn visible_rows(
        &self,
        columns: usize,
        capacity_rows: usize,
    ) -> Vec<Vec<IconPickerCellView>> {
        self.columns.set(columns);
        let columns = columns.max(1);
        let capacity_rows = capacity_rows.max(1);
        let offered = self.offered();
        if offered.is_empty() {
            return Vec::new();
        }
        let rows = grouped_rows(&offered, columns);
        let shown = self
            .window
            .settle_rows(rows.len(), capacity_rows, self.row_of_selected(&rows));
        rows[shown]
            .iter()
            .map(|row| {
                row.iter()
                    .map(|entry| IconPickerCellView {
                        entry,
                        selected: Some(entry.name) == self.selected,
                    })
                    .collect()
            })
            .collect()
    }

    /// Gives up the last frame's geometry, called as every frame begins, so a
    /// press resolves only against cells actually on screen.
    pub(super) fn forget_frame(&self) {
        self.geometry.borrow_mut().clear();
    }

    /// Records where the frame in force drew one cell.
    pub(super) fn record_cell(&self, row: u16, columns: Range<u16>, name: &'static str) {
        self.geometry
            .borrow_mut()
            .push(IconPickerCellGeometry { row, columns, name });
    }

    /// The Catalog name whose cell the reader pressed, or `None` for a press
    /// outside every cell.
    pub(super) fn hit(&self, position: Position) -> Option<&'static str> {
        self.geometry
            .borrow()
            .iter()
            .find(|cell| cell.row == position.y && cell.columns.contains(&position.x))
            .map(|cell| cell.name)
    }

    /// The Catalog entries the current query offers, in listing order — every
    /// Subject before every Technology. An empty query is the whole Catalog:
    /// [`icon_catalog::search`] already answers that without a separate path.
    fn offered(&self) -> Vec<&'static IconEntry> {
        icon_catalog::search(&self.query)
    }

    fn selected_index(&self, offered: &[&'static IconEntry]) -> Option<usize> {
        self.selected
            .and_then(|name| offered.iter().position(|entry| entry.name == name))
    }

    /// Which of `rows` the reader's cell falls in, `None` while nothing is
    /// focused — a query that offers nothing, chiefly.
    fn row_of_selected(&self, rows: &[Vec<&'static IconEntry>]) -> Option<usize> {
        let selected = self.selected?;
        rows.iter()
            .position(|row| row.iter().any(|entry| entry.name == selected))
    }

    fn focus_first(&mut self) {
        self.selected = self.offered().first().map(|entry| entry.name);
    }

    fn move_horizontal(&mut self, direction: isize) {
        let offered = self.offered();
        if offered.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index(&offered).unwrap_or(0) as isize;
        let last = offered.len() as isize - 1;
        let next = (current + direction).clamp(0, last) as usize;
        self.selected = Some(offered[next].name);
        self.window.reveal();
    }

    /// Moves focus one row up or down, staying in the same column where the
    /// destination row has one — a row's own [`IconGroup`] boundary can leave
    /// it shorter than `columns`, so the column is clamped into whatever that
    /// row actually offers rather than assumed uniform.
    ///
    /// [`IconGroup`]: crate::icon_catalog::IconGroup
    fn move_vertical(&mut self, direction: isize) {
        let offered = self.offered();
        if offered.is_empty() {
            self.selected = None;
            return;
        }
        let columns = self.columns.get().max(1);
        let rows = grouped_rows(&offered, columns);
        let (row, column) = self
            .row_of_selected(&rows)
            .and_then(|row| {
                let selected = self.selected?;
                let column = rows[row].iter().position(|entry| entry.name == selected)?;
                Some((row, column))
            })
            .unwrap_or((0, 0));
        let last_row = rows.len().saturating_sub(1) as isize;
        let next_row = (row as isize + direction).clamp(0, last_row) as usize;
        let target = &rows[next_row];
        let column = column.min(target.len().saturating_sub(1));
        self.selected = Some(target[column].name);
        self.window.reveal();
    }
}

/// How many glyph cells fit across one row of `width` terminal columns, never
/// fewer than one: a picker narrower than a single cell still shows the grid
/// one cell at a time rather than none at all.
pub(super) fn columns_for_width(width: u16) -> usize {
    usize::from((width + CELL_GUTTER) / (CELL_WIDTH + CELL_GUTTER)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Outlook, SessionId};

    fn target() -> IconPickerTarget {
        IconPickerTarget::Session(SessionReference::new(Outlook::Local, SessionId::new()))
    }

    #[test]
    fn opening_focuses_the_first_catalog_entry() {
        let mut picker = IconPicker::default();
        picker.open(target());
        assert!(picker.is_open());
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().first().map(|entry| entry.name)
        );
    }

    #[test]
    fn closing_clears_the_target_and_query() {
        let mut picker = IconPicker::default();
        picker.open(target());
        picker.insert("bug");
        picker.close();
        assert!(!picker.is_open());
        assert_eq!(picker.query(), "");
        assert!(picker.focused_entry().is_none());
    }

    #[test]
    fn a_query_narrows_the_grid_and_refocuses_the_first_result() {
        let mut picker = IconPicker::default();
        picker.open(target());
        picker.insert("rust");
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            Some("dev-rust")
        );
    }

    #[test]
    fn deleting_widens_the_query_and_refocuses_the_first_result() {
        let mut picker = IconPicker::default();
        picker.open(target());
        picker.insert("rustx");
        assert!(picker.focused_entry().is_none(), "no entry matches `rustx`");
        picker.delete_backward();
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            Some("dev-rust")
        );
    }

    #[test]
    fn horizontal_movement_clamps_at_the_grid_edges() {
        let mut picker = IconPicker::default();
        picker.open(target());
        picker.move_left();
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().first().map(|entry| entry.name),
            "moving left from the first cell stays on it"
        );
        for _ in 0..icon_catalog::entries().len() + 5 {
            picker.move_right();
        }
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().last().map(|entry| entry.name),
            "moving right past the last cell clamps to it"
        );
    }

    #[test]
    fn vertical_movement_steps_by_the_last_drawn_column_count() {
        let mut picker = IconPicker::default();
        picker.open(target());
        // Lay out a grid three cells wide, as a draw would before any
        // movement, so Down has a row width to step by.
        let _ = picker.visible_rows(3, 4);
        picker.move_down();
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().get(3).map(|entry| entry.name)
        );
        picker.move_up();
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().first().map(|entry| entry.name)
        );
    }

    #[test]
    fn moving_up_from_the_first_row_clamps_rather_than_wraps() {
        let mut picker = IconPicker::default();
        picker.open(target());
        let _ = picker.visible_rows(3, 4);
        picker.move_up();
        assert_eq!(
            picker.focused_entry().map(|entry| entry.name),
            icon_catalog::entries().first().map(|entry| entry.name)
        );
    }

    #[test]
    fn hit_testing_answers_only_the_last_recorded_frame() {
        let picker = IconPicker::default();
        picker.record_cell(2, 4..7, "md-bug");
        assert_eq!(picker.hit(Position::new(5, 2)), Some("md-bug"));
        assert_eq!(picker.hit(Position::new(8, 2)), None);
        picker.forget_frame();
        assert_eq!(picker.hit(Position::new(5, 2)), None);
    }

    #[test]
    fn columns_for_width_never_reports_zero() {
        assert_eq!(columns_for_width(0), 1);
        assert_eq!(columns_for_width(3), 1, "one cell needs exactly 3 columns");
        assert_eq!(
            columns_for_width(6),
            1,
            "not yet room for a second cell's gutter"
        );
        assert_eq!(
            columns_for_width(7),
            2,
            "two cells need exactly 3 + 1 + 3 columns"
        );
        assert_eq!(columns_for_width(10), 2);
        assert_eq!(
            columns_for_width(11),
            3,
            "three cells need exactly 11 columns"
        );
    }
}
