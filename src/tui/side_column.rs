//! The side column: the chrome a collapsible column beside the main view wears,
//! whichever side of it the column stands on.
//!
//! The Sidebar stands on the left and the Aside on the right. What they list
//! is their own; what they share is here — the width the reader chose and the
//! width the last frame could actually draw, the two floors that clamp it, the
//! edge the reader drags, whether the column is shown, and whether it holds
//! the keys. The frame's layout of both columns around the main view lives
//! here too, in [`lay_out`], because the squeeze is a rule between columns
//! rather than something either one decides alone.

use std::{cell::Cell, ops::Range};

use ratatui::{
    layout::{Position, Rect},
    widgets::Borders,
};

/// The narrowest side column a reader can use. The chosen width may be
/// greater, but draw-time clamping never lets a narrow frame overwrite that
/// choice.
pub(super) const MINIMUM_COLUMN_WIDTH: u16 = 24;

/// The narrowest main view a side column will leave behind: the 50 columns a
/// reader is allowed to cap the Session Content Column at (ADR 0012), inside
/// the two columns of padding the frame insets it by. Below that a column
/// would be buying its own columns out of the conversation.
pub(super) const MINIMUM_MAIN_WIDTH: u16 = 54;

/// The width a side column begins at before any Setting has had its say.
const DEFAULT_COLUMN_WIDTH: u64 = 32;

/// Which side of the main view a column stands on. The side decides where its
/// rule is drawn, where the grab zone of its edge lies, and which way dragging
/// that edge widens it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Side {
    /// Left of the main view, with its rule down its right.
    Left,
    /// Right of the main view, with its rule down its left.
    Right,
}

impl Side {
    /// The border a column on this side draws as its rule: the one facing the
    /// main view.
    pub(super) const fn rule(self) -> Borders {
        match self {
            Self::Left => Borders::RIGHT,
            Self::Right => Borders::LEFT,
        }
    }
}

/// What one press of a column's show/hide act does, decided by where the
/// column stands. The act brings the reader into the column as well as
/// showing it, so reaching a column already on screen never costs the reader
/// its place: only a column already holding the keys hides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum ToggleStep {
    /// The column was hidden: show it and give it the keys.
    Show,
    /// The column was shown without the keys: give it them and keep it shown.
    TakeKeys,
    /// The column held the keys: hide it and hand them back.
    Hide,
}

/// One column's chrome.
///
/// Visibility has two independent halves. The reader's choice — seeded once
/// from a Setting and moved by the show/hide act — lives here and is never
/// written back to configuration. Whether the frame can actually spare the
/// columns is decided at draw time by [`lay_out`], so a terminal that
/// squeezes the column out forgets nothing.
#[derive(Clone, Debug)]
pub(super) struct SideColumn {
    side: Side,
    revealed: bool,
    /// Whether the reader is driving the column rather than the composer.
    /// This is the column's claim on the keys; whether the keys actually
    /// reach it also depends on the frame having drawn it.
    focused: bool,
    /// Whether the Settings that seed the column have had their say. Only the
    /// first snapshot seeds: every later one carries some other Setting's
    /// edit, and a reader who moved the column since should not have it moved
    /// back under them.
    seeded: bool,
    /// The width the reader wants, independently of how many columns the
    /// current frame can spare. Seeded once from the launch Setting; a draw
    /// clamps a copy and leaves this choice whole.
    chosen_width: u64,
    /// Where the last frame drew the column, rule included. Incremental
    /// commands move the visible edge from its width, so they stop at both
    /// draw-time floors and a command after a terminal clamp still moves one
    /// visible column.
    drawn: Cell<Option<Rect>>,
    /// The widest the last frame could draw the column while keeping the main
    /// view at its floor. Kept with the drawn area so widening at that
    /// boundary does not create a latent choice that appears only after a
    /// later resize.
    drawn_width_limit: Cell<Option<u16>>,
    /// Whether the last frame had the columns to draw this one. Only a frame
    /// can answer that, so each one records it here and input routing reads
    /// it back: a column squeezed off a narrow terminal keeps the reader's
    /// focus but cannot act on it.
    on_screen: Cell<bool>,
    /// Whether the reader is holding the column's edge. This is pointer
    /// interaction state, separate from both key ownership and row focus.
    edge_held: bool,
}

impl SideColumn {
    /// A hidden, unseeded column on one side of the main view. A column with
    /// no Settings in hand has not spoken to a server either, so it waits for
    /// the Setting that raises it rather than drawing an empty column and
    /// taking it away again for a reader who configured it hidden.
    pub(super) const fn new(side: Side) -> Self {
        Self {
            side,
            revealed: false,
            focused: false,
            seeded: false,
            chosen_width: DEFAULT_COLUMN_WIDTH,
            drawn: Cell::new(None),
            drawn_width_limit: Cell::new(None),
            // Until a frame says otherwise, which it does before anything the
            // reader types can reach a surface.
            on_screen: Cell::new(true),
            edge_held: false,
        }
    }

    pub(super) const fn side(&self) -> Side {
        self.side
    }

    /// Takes the launch width from the first Settings snapshot, and reports
    /// whether this was that first snapshot — the one whose initial
    /// visibility the owner should then honour. Later snapshots leave the
    /// reader's own choices alone.
    pub(super) fn seed(&mut self, initial_width: u64) -> bool {
        if self.seeded {
            return false;
        }
        self.seeded = true;
        self.chosen_width = initial_width;
        true
    }

    /// Whether the reader wants the column on screen, which is not the same
    /// question as whether the frame has room for it.
    pub(super) const fn is_revealed(&self) -> bool {
        self.revealed
    }

    /// Shows the column or hides it. This is view state and nothing more: the
    /// initial-visibility Setting is not rewritten, and neither is the claim
    /// on the keys — taking or handing them back is its own step.
    pub(super) fn set_revealed(&mut self, revealed: bool) {
        self.revealed = revealed;
        if revealed {
            // A reader opening the column can type into it before the next
            // frame is drawn: the run loop takes a whole run of terminal
            // events at once, so the toggle and the arrow after it are
            // handled with no draw between them. The column assumes it has
            // the columns until a frame reports otherwise, so that run reaches
            // the surface the reader just opened.
            self.on_screen.set(true);
        } else {
            self.edge_held = false;
        }
    }

    /// What the show/hide act would do from here. The owner carries it out,
    /// because showing, entering and leaving each mean more to a column than
    /// its chrome.
    ///
    /// A column shown but squeezed off the frame still holds its claim on the
    /// keys, so the act hides it: the reader can always put away a column
    /// they cannot see, rather than taking keys it cannot use.
    pub(super) const fn toggle_step(&self) -> ToggleStep {
        if !self.revealed {
            ToggleStep::Show
        } else if self.focused {
            ToggleStep::Hide
        } else {
            ToggleStep::TakeKeys
        }
    }

    /// Stakes the column's claim on the keys.
    pub(super) fn take_keys(&mut self) {
        self.focused = true;
    }

    /// Gives up the column's claim on the keys.
    pub(super) fn hand_back_keys(&mut self) {
        self.focused = false;
    }

    /// Whether the reader has given the column the keys, whether or not the
    /// last frame found room to draw it.
    pub(super) const fn claims_keys(&self) -> bool {
        self.focused
    }

    /// Whether the reader is driving the column. A column the frame could not
    /// spare the columns for is not one they can be driving, whatever they
    /// last asked for, so the composer keeps the keys until the terminal
    /// widens.
    pub(super) fn has_keys(&self) -> bool {
        self.focused && self.on_screen.get()
    }

    /// Whether the last frame drew the column.
    pub(super) fn is_on_screen(&self) -> bool {
        self.on_screen.get()
    }

    pub(super) const fn chosen_width(&self) -> u64 {
        self.chosen_width
    }

    /// Sets a freely chosen width. The column's floor is part of every valid
    /// choice; the frame-dependent upper clamp remains a draw-time concern so
    /// a wider terminal can reveal the choice whole later.
    pub(super) fn set_width(&mut self, columns: u64) {
        self.chosen_width = columns.max(u64::from(MINIMUM_COLUMN_WIDTH));
    }

    /// Makes the drawn column one column wider where the current frame has
    /// room.
    pub(super) fn widen(&mut self) {
        if !self.revealed {
            return;
        }
        let Some(drawn) = self.drawn_width() else {
            return;
        };
        let Some(limit) = self.drawn_width_limit.get() else {
            return;
        };
        if drawn < limit {
            self.chosen_width = u64::from(drawn + 1);
        }
    }

    /// Makes the drawn column one column narrower, stopping at its floor.
    pub(super) fn narrow(&mut self) {
        if !self.revealed {
            return;
        }
        let Some(drawn) = self.drawn_width() else {
            return;
        };
        self.chosen_width = u64::from(drawn.saturating_sub(1).max(MINIMUM_COLUMN_WIDTH));
    }

    /// Where the last frame drew the column, rule included, or `None` for a
    /// frame that did not draw it.
    pub(super) fn drawn_area(&self) -> Option<Rect> {
        self.drawn.get()
    }

    fn drawn_width(&self) -> Option<u16> {
        self.drawn.get().map(|area| area.width)
    }

    /// Gives up what the last frame recorded, so the geometry input routing
    /// reads is always the one on screen.
    pub(super) fn forget_frame(&self) {
        self.on_screen.set(false);
        self.drawn.set(None);
        self.drawn_width_limit.set(None);
    }

    /// Records that this frame found the columns for the column and drew it
    /// across `area`, rule included, under a width limit of `width_limit`.
    pub(super) fn record_drawn(&self, area: Rect, width_limit: u16) {
        self.on_screen.set(true);
        self.drawn.set(Some(area));
        self.drawn_width_limit.set(Some(width_limit));
    }

    /// The edge's three-column grab zone: the rule and one column on either
    /// side. It exists only for a frame that drew the column beside the main
    /// view.
    fn edge(&self) -> Option<Range<u16>> {
        let area = self.drawn.get()?;
        Some(match self.side {
            Side::Left => area.right().saturating_sub(2)..area.right().saturating_add(1),
            Side::Right => area.x.saturating_sub(1)..area.x.saturating_add(2),
        })
    }

    /// Begins holding the edge where the last frame drew its grab zone.
    /// Returns false when that frame drew no column or the press missed it.
    pub(super) fn hold_edge_at(&mut self, position: Position) -> bool {
        let hit = self.edge().is_some_and(|edge| edge.contains(&position.x));
        if hit {
            self.edge_held = true;
        }
        hit
    }

    pub(super) const fn edge_is_held(&self) -> bool {
        self.edge_held
    }

    /// Resolves a held edge's pointer column to a chosen width under the last
    /// frame's two floors. The column includes its rule, so the width puts
    /// that rule directly under the pointer. Unlike an explicit set-width
    /// command, a drag stops at the room that frame actually offered rather
    /// than keeping an over-limit choice for a later, wider frame.
    pub(super) fn width_at_held_edge(&self, position: Position) -> Option<u64> {
        if !self.edge_held {
            return None;
        }
        let area = self.drawn.get()?;
        let limit = u64::from(self.drawn_width_limit.get()?);
        let reach = match self.side {
            Side::Left => (u64::from(position.x) + 1).saturating_sub(u64::from(area.x)),
            Side::Right => u64::from(area.right()).saturating_sub(u64::from(position.x)),
        };
        Some(reach.max(u64::from(MINIMUM_COLUMN_WIDTH)).min(limit))
    }

    /// Whether this cell is the first one beyond the grab zone in the main
    /// view. The Session Content Column begins after layout padding, but this
    /// boundary cell retains the ordinary Text Selection behavior promised
    /// immediately outside the edge.
    pub(super) fn borders_edge_on_main_side(&self, position: Position) -> bool {
        self.edge().is_some_and(|edge| match self.side {
            Side::Left => position.x == edge.end,
            Side::Right => position.x.checked_add(1) == Some(edge.start),
        })
    }

    /// Releases a held edge and reports whether this release belonged to it.
    pub(super) fn release_edge(&mut self) -> bool {
        std::mem::take(&mut self.edge_held)
    }
}

/// How one frame divides its width between the Sidebar, the main view, and
/// the right-hand column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FrameColumns {
    pub(super) left: Option<Rect>,
    pub(super) main: Rect,
    pub(super) right: Option<Rect>,
}

/// One column's share of a frame, as [`widths`] decides it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ColumnWidth {
    /// The columns drawn, rule included.
    pub(super) width: u16,
    /// The widest this frame could have drawn the column while keeping the
    /// main view at its floor.
    pub(super) limit: u16,
}

/// Divides a frame this wide between a left column that wants `left` columns,
/// a main view, and a right column that wants `right`, where `None` is a
/// column the reader has not shown.
///
/// This is the whole of the squeeze. The main view's floor outranks both
/// columns. Where the terminal cannot keep all three, the right column gives
/// way first — narrowing to its floor, then leaving the frame — and the left
/// column only after it, because the Sidebar is how the reader moves between
/// their work and the right column only reads more of it. A column squeezed
/// out keeps the reader's choice, so widening the terminal brings it back
/// exactly as they left it.
pub(super) fn widths(
    frame_width: u16,
    left: Option<u64>,
    right: Option<u64>,
) -> (Option<ColumnWidth>, Option<ColumnWidth>) {
    let left = left.and_then(|chosen| width_beside(chosen, frame_width));
    let rest = frame_width - left.map_or(0, |left| left.width);
    let right = right.and_then(|chosen| width_beside(chosen, rest));
    (left, right)
}

/// The columns one side column takes from `room` columns shared with the main
/// view, and `None` where that room cannot spare them.
const fn width_beside(chosen_width: u64, room: u16) -> Option<ColumnWidth> {
    if room < MINIMUM_COLUMN_WIDTH + MINIMUM_MAIN_WIDTH {
        return None;
    }
    let limit = room - MINIMUM_MAIN_WIDTH;
    let chosen = if chosen_width > u16::MAX as u64 {
        u16::MAX
    } else {
        chosen_width as u16
    };
    let width = if chosen < MINIMUM_COLUMN_WIDTH {
        MINIMUM_COLUMN_WIDTH
    } else if chosen > limit {
        limit
    } else {
        chosen
    };
    Some(ColumnWidth { width, limit })
}

/// Lays a frame out as left column | main view | right column, and records on
/// each column drawn where it went. A column passed as `None`, or one the
/// reader has hidden, takes nothing; one the frame cannot spare the columns
/// for is recorded as not on screen, having been forgotten when the frame
/// began.
pub(super) fn lay_out(
    area: Rect,
    left: Option<&SideColumn>,
    right: Option<&SideColumn>,
) -> FrameColumns {
    let left = left.filter(|column| column.is_revealed());
    let right = right.filter(|column| column.is_revealed());
    let (left_width, right_width) = widths(
        area.width,
        left.map(SideColumn::chosen_width),
        right.map(SideColumn::chosen_width),
    );
    let mut main = area;
    let left = left.zip(left_width).map(|(column, share)| {
        let drawn = Rect {
            width: share.width,
            ..area
        };
        column.record_drawn(drawn, share.limit);
        main.x += share.width;
        main.width -= share.width;
        drawn
    });
    let right = right.zip(right_width).map(|(column, share)| {
        let drawn = Rect {
            x: area.right() - share.width,
            width: share.width,
            ..area
        };
        column.record_drawn(drawn, share.limit);
        main.width -= share.width;
        drawn
    });
    FrameColumns { left, main, right }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(width: u16, limit: u16) -> Option<ColumnWidth> {
        Some(ColumnWidth { width, limit })
    }

    fn shown(side: Side, width: u64) -> SideColumn {
        let mut column = SideColumn::new(side);
        column.seed(width);
        column.set_revealed(true);
        column
    }

    #[test]
    fn the_drawn_width_clamps_without_changing_the_chosen_width() {
        assert_eq!(widths(100, Some(40), None).0, share(40, 46));
        assert_eq!(widths(100, Some(60), None).0, share(46, 46));
        assert_eq!(widths(78, Some(60), None).0, share(24, 24));
        assert_eq!(widths(77, Some(60), None).0, None);
        assert_eq!(
            widths(u16::MAX, Some(1_000_000), None).0,
            share(65_481, 65_481)
        );
        assert_eq!(widths(100, Some(1), None).0, share(24, 46));
    }

    #[test]
    fn a_right_column_alone_clamps_as_the_sidebar_does() {
        assert_eq!(widths(100, None, Some(40)), (None, share(40, 46)));
        assert_eq!(widths(77, None, Some(40)), (None, None));
    }

    #[test]
    fn all_three_fit_from_the_sum_of_both_columns_and_the_main_floor() {
        assert_eq!(
            widths(118, Some(32), Some(32)),
            (share(32, 64), share(32, 32))
        );
    }

    #[test]
    fn the_right_column_narrows_to_its_floor_before_the_sidebar_gives_anything() {
        assert_eq!(
            widths(110, Some(32), Some(32)),
            (share(32, 56), share(24, 24))
        );
    }

    #[test]
    fn the_right_column_leaves_before_the_sidebar_narrows() {
        assert_eq!(widths(109, Some(32), Some(32)), (share(32, 55), None));
        assert_eq!(widths(78, Some(32), Some(32)), (share(24, 24), None));
    }

    #[test]
    fn the_main_floor_outlasts_both_columns() {
        assert_eq!(widths(77, Some(32), Some(32)), (None, None));
        assert_eq!(
            widths(78, None, Some(32)),
            (None, share(24, 24)),
            "a hidden Sidebar leaves its columns to the right column"
        );
    }

    #[test]
    fn a_squeezed_frame_forgets_neither_column_and_growth_restores_both() {
        let left = shown(Side::Left, 40);
        let right = shown(Side::Right, 36);
        let frame = |width| Rect::new(0, 0, width, 10);

        left.forget_frame();
        right.forget_frame();
        let squeezed = lay_out(frame(90), Some(&left), Some(&right));
        assert_eq!(squeezed.right, None);
        assert!(left.is_on_screen() && !right.is_on_screen());
        assert_eq!(left.chosen_width(), 40);
        assert_eq!(right.chosen_width(), 36);

        left.forget_frame();
        right.forget_frame();
        let whole = lay_out(frame(130), Some(&left), Some(&right));
        assert_eq!(whole.left, Some(Rect::new(0, 0, 40, 10)));
        assert_eq!(whole.main, Rect::new(40, 0, 54, 10));
        assert_eq!(whole.right, Some(Rect::new(94, 0, 36, 10)));
        assert!(left.is_on_screen() && right.is_on_screen());
    }

    #[test]
    fn a_hidden_column_takes_nothing_from_the_frame() {
        let left = SideColumn::new(Side::Left);
        let right = shown(Side::Right, 32);
        let frame = Rect::new(0, 0, 120, 10);

        let columns = lay_out(frame, Some(&left), Some(&right));

        assert_eq!(columns.left, None);
        assert_eq!(columns.main, Rect::new(0, 0, 88, 10));
        assert_eq!(columns.right, Some(Rect::new(88, 0, 32, 10)));
    }

    #[test]
    fn the_toggle_shows_then_takes_the_keys_then_hides() {
        let mut column = SideColumn::new(Side::Right);
        assert_eq!(column.toggle_step(), ToggleStep::Show);

        column.set_revealed(true);
        assert_eq!(column.toggle_step(), ToggleStep::TakeKeys);

        column.take_keys();
        assert_eq!(column.toggle_step(), ToggleStep::Hide);

        column.forget_frame();
        assert_eq!(
            column.toggle_step(),
            ToggleStep::Hide,
            "a column squeezed off the frame can still be put away"
        );
    }

    #[test]
    fn only_the_first_settings_snapshot_seeds_the_width() {
        let mut column = SideColumn::new(Side::Left);
        assert!(column.seed(40));
        column.set_width(50);
        assert!(!column.seed(60));
        assert_eq!(column.chosen_width(), 50);
    }

    #[test]
    fn a_right_edge_is_dragged_from_the_frames_right_and_stops_at_both_floors() {
        let left = shown(Side::Left, 32);
        let mut right = shown(Side::Right, 32);
        lay_out(Rect::new(0, 0, 130, 10), Some(&left), Some(&right));
        // The right column stands at 98..130, its rule at 98.
        assert!(!right.hold_edge_at(Position::new(96, 0)));
        assert!(right.hold_edge_at(Position::new(97, 0)));
        assert!(right.borders_edge_on_main_side(Position::new(96, 0)));
        assert!(!right.borders_edge_on_main_side(Position::new(97, 0)));

        assert_eq!(right.width_at_held_edge(Position::new(90, 0)), Some(40));
        assert_eq!(
            right.width_at_held_edge(Position::new(120, 0)),
            Some(24),
            "the column's own floor"
        );
        assert_eq!(
            right.width_at_held_edge(Position::new(10, 0)),
            Some(44),
            "the main view's floor beside the Sidebar"
        );
        assert!(right.release_edge());
        assert_eq!(right.width_at_held_edge(Position::new(90, 0)), None);
    }

    #[test]
    fn a_left_edge_is_dragged_from_the_frames_left() {
        let mut left = shown(Side::Left, 32);
        lay_out(Rect::new(0, 0, 120, 10), Some(&left), None);
        // The Sidebar stands at 0..32, its rule at 31.
        assert!(!left.hold_edge_at(Position::new(33, 0)));
        assert!(left.hold_edge_at(Position::new(32, 0)));
        assert!(left.borders_edge_on_main_side(Position::new(33, 0)));
        assert_eq!(left.width_at_held_edge(Position::new(39, 0)), Some(40));
        assert_eq!(left.width_at_held_edge(Position::new(3, 0)), Some(24));
        assert_eq!(left.width_at_held_edge(Position::new(110, 0)), Some(66));
    }

    #[test]
    fn widening_and_narrowing_move_the_drawn_edge_within_the_frame() {
        let mut column = shown(Side::Right, 60);
        lay_out(Rect::new(0, 0, 100, 10), None, Some(&column));
        column.widen();
        assert_eq!(
            column.chosen_width(),
            60,
            "a column drawn at its limit has nowhere to widen to"
        );
        column.narrow();
        assert_eq!(column.chosen_width(), 45, "narrowing moves the drawn edge");

        column.set_revealed(false);
        column.narrow();
        assert_eq!(column.chosen_width(), 45, "a hidden column is not resized");
    }
}
