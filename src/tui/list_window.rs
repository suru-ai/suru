//! The window a list the reader drives from the keys is read through, and the
//! margin it keeps around Row Focus.
//!
//! Every such list — the pickers, the completion popup, the settings panel,
//! the Sidebar and the Aside's Sections — is often longer than the box it is
//! drawn in, so it shows a window of its entries. Where that window stands is
//! remembered between frames rather than worked out afresh from the focused
//! entry each time, because a window worked out afresh can only pin focus to
//! one of its edges: walking back up a list whose focus was carried to its
//! foot would drag the whole list along with it.

use std::{
    cell::{Cell, RefCell},
    ops::Range,
};

/// How many entries the keys keep in view beyond row focus, ahead and behind,
/// where the window is tall enough to spare them.
const MARGIN: usize = 2;

/// One entry of a list as its window measures it: the Rows it takes, and
/// whether row focus can stand on it. The margin is counted in entries focus
/// could stand on, so a Provider heading or a divider between them comes into
/// view along with them rather than counting towards it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowEntry {
    pub(super) rows: usize,
    pub(super) focusable: bool,
}

impl WindowEntry {
    /// An entry of one Row that focus can stand on, which is what most lists
    /// are made of.
    pub(super) const ROW: Self = Self {
        rows: 1,
        focusable: true,
    };

    /// An entry of `rows` Rows that focus can stand on.
    pub(super) const fn focusable(rows: usize) -> Self {
        Self {
            rows,
            focusable: true,
        }
    }

    /// An entry of `rows` Rows the keys step over.
    pub(super) const fn passive(rows: usize) -> Self {
        Self {
            rows,
            focusable: false,
        }
    }
}

/// Where a list's window stands, kept between frames.
///
/// Only a frame knows how many Rows the list has to be drawn in, so the
/// window is settled as it is drawn, by [`Self::settle`]. What the list tells
/// it in between is why focus moved:
///
/// - [`Self::open`] when the list opens, or becomes another list — a new
///   query, another scope. It opens at its head and is carried to focus from
///   there, as though the keys had walked it.
/// - [`Self::reveal`] when the keys move focus. The next frame carries the
///   window only as far as keeps [`MARGIN`] entries beyond focus in view.
/// - [`Self::scroll_to`] when the reader moves the window themselves, as the
///   wheel does.
/// - [`Self::hold`] when the pointer moved focus through a path the keys share,
///   taking back the reveal that path asked for.
///
/// Anything else — the entries changing under focus, a terminal resizing —
/// tells it nothing, and the window holds where it stands until the keys move
/// focus again. The one exception is a window growing shorter under the focus
/// it was showing, which moves only as far as keeps that focus in view.
#[derive(Clone, Debug, Default)]
pub(super) struct ListWindow {
    /// The entry the window opens on.
    first: Cell<usize>,
    /// What the next frame to draw focus should do with the window.
    carry: Cell<Carry>,
    /// The Rows the window was last settled over.
    capacity: Cell<usize>,
}

/// What a list that follows the open Session's entry remembers between frames
/// to tell why that entry changed: which entry it was when the last frame was
/// drawn, and whether a press has landed since.
///
/// Another Session opening carries the window to its entry as the keys would
/// carry it. One the reader opened by pressing its entry was already in view,
/// so that opening — and whatever else the press moved — moves no window.
#[derive(Clone, Debug)]
pub(super) struct OpenEntry<K> {
    /// The open Session's entry as the last frame drew it.
    drawn: RefCell<Option<K>>,
    /// Whether a press has landed on the list since the last frame.
    pressed: Cell<bool>,
}

impl<K> Default for OpenEntry<K> {
    fn default() -> Self {
        Self {
            drawn: RefCell::new(None),
            pressed: Cell::new(false),
        }
    }
}

impl<K: Clone + PartialEq> OpenEntry<K> {
    /// Notes a press on the list, which the next frame answers by holding.
    pub(super) fn press(&self) {
        self.pressed.set(true);
    }

    /// Tells `window` what the frame about to draw `open` as the open
    /// Session's entry asks of it: to hold where a press has landed since the
    /// last frame, and otherwise to reveal the entry where another Session has
    /// opened.
    pub(super) fn follow(&self, window: &ListWindow, open: Option<&K>) {
        let opened = self.drawn.replace(open.cloned()).as_ref() != open;
        if self.pressed.take() {
            window.hold();
        } else if opened {
            window.reveal();
        }
    }
}

/// How far a window is waiting to be carried to focus.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Carry {
    /// Nowhere: it holds where it stands.
    #[default]
    Hold,
    /// As far as the keys carry it.
    Reveal,
    /// From the list's head, as far as the keys would carry it from there.
    Open,
}

impl ListWindow {
    /// Opens the window at the head of the list, to be carried to focus by
    /// the frame that first draws it with focus on an entry.
    pub(super) fn open(&self) {
        self.first.set(0);
        self.carry.set(Carry::Open);
    }

    /// Asks the next frame to carry the window to focus, which the keys
    /// moving it do.
    pub(super) fn reveal(&self) {
        if self.carry.get() == Carry::Hold {
            self.carry.set(Carry::Reveal);
        }
    }

    /// Takes back a reveal no frame has drawn yet, which the pointer moving
    /// focus does: what it pointed at is already in view. A list opening is
    /// not taken back, since it opens on focus however it was asked for.
    pub(super) fn hold(&self) {
        if self.carry.get() == Carry::Reveal {
            self.carry.set(Carry::Hold);
        }
    }

    /// Moves the window to open on `first`, where the reader put it, and holds
    /// it there.
    pub(super) fn scroll_to(&self, first: usize) {
        self.first.set(first);
        self.carry.set(Carry::Hold);
    }

    /// The entry the window opened on when it was last settled.
    pub(super) fn first(&self) -> usize {
        self.first.get()
    }

    /// The Rows the window was last settled over, which a page of it is.
    pub(super) fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// Settles the window over `entries` for a frame holding `capacity` Rows
    /// with focus on the entry at `focus`, and answers the entries it shows.
    ///
    /// A window asked to reveal focus is carried as the keys would carry it
    /// (see [`carried`]); a list with no entries yet, or no focus among them,
    /// keeps the ask for the frame that has them, so a list still loading
    /// opens on its focus once it lands. A window asked for nothing holds,
    /// unless it has grown shorter than the last frame's and would lose the
    /// focus that frame showed. Either way the window never opens so far down
    /// that it trails blank Rows below a list that has since grown shorter.
    ///
    /// The window shows every entry from its first that fits whole, and at
    /// least its first — cut short — where that one alone is taller than the
    /// window, because a window showing nothing would be worse than one
    /// showing what it can.
    pub(super) fn settle(
        &self,
        entries: &[WindowEntry],
        capacity: usize,
        focus: Option<usize>,
    ) -> Range<usize> {
        if capacity == 0 || entries.is_empty() {
            return 0..0;
        }
        let last_capacity = self.capacity.replace(capacity);
        let mut first = self.first.get().min(furthest_opening(entries, capacity));
        let focus = focus.filter(|focus| *focus < entries.len());
        if self.carry.get() != Carry::Hold
            && let Some(focus) = focus
        {
            first = carried(entries, capacity, first, focus);
            self.carry.set(Carry::Hold);
        } else if let Some(focus) = focus
            && capacity < last_capacity
            && focus < window_end(entries, first, last_capacity)
            && focus >= window_end(entries, first, capacity)
        {
            first = earliest_opening(entries, focus, capacity);
        }
        self.first.set(first);
        first..window_end(entries, first, capacity)
    }

    /// [`Self::settle`] for a list whose entries are every one a single Row
    /// focus can stand on.
    pub(super) fn settle_rows(
        &self,
        len: usize,
        capacity: usize,
        focus: Option<usize>,
    ) -> Range<usize> {
        self.settle(&vec![WindowEntry::ROW; len], capacity, focus)
    }

    /// The rows of a list of single-Row entries that its window shows this
    /// frame.
    pub(super) fn show<T>(
        &self,
        rows: Vec<T>,
        capacity: usize,
        focus: Option<usize>,
    ) -> impl Iterator<Item = T> {
        let shown = self.settle_rows(rows.len(), capacity, focus);
        rows.into_iter().skip(shown.start).take(shown.len())
    }
}

/// The furthest entry a window of `capacity` Rows can open on without trailing
/// blank Rows below the list's last entry, which is as far as anything —
/// the keys or the wheel — may move it.
pub(super) fn furthest_opening(entries: &[WindowEntry], capacity: usize) -> usize {
    entries
        .len()
        .checked_sub(1)
        .map_or(0, |last| earliest_opening(entries, last, capacity))
}

/// Where a window opening on `first` stands once the keys have carried focus
/// to `focus`: moved only as far as keeps `margin` entries beyond focus in
/// view on either side, and not at all while the list's end on that side
/// already shows.
///
/// The margin is [`MARGIN`] where the window can hold that many either side of
/// focus, and as many as it can spare evenly where it cannot, so a short
/// window still lets focus move without the list jumping under every step.
fn carried(entries: &[WindowEntry], capacity: usize, first: usize, focus: usize) -> usize {
    let (behind, ahead) = (0..=MARGIN)
        .rev()
        .map(|margin| {
            (
                margin_behind(entries, focus, margin),
                margin_ahead(entries, focus, margin),
            )
        })
        .find(|(behind, ahead)| {
            behind == ahead || rows_between(entries, *behind, *ahead) <= capacity
        })
        .unwrap_or((focus, focus));
    if behind < first {
        return behind;
    }
    first.max(earliest_opening(entries, ahead, capacity))
}

/// The entry `margin` focusable entries behind `focus`, or the list's head
/// where fewer than that stand behind it.
fn margin_behind(entries: &[WindowEntry], focus: usize, margin: usize) -> usize {
    if margin == 0 {
        return focus;
    }
    (0..focus)
        .rev()
        .filter(|index| entries[*index].focusable)
        .nth(margin - 1)
        .unwrap_or(0)
}

/// The entry `margin` focusable entries ahead of `focus`, or the list's last
/// where fewer than that stand ahead of it.
fn margin_ahead(entries: &[WindowEntry], focus: usize, margin: usize) -> usize {
    if margin == 0 {
        return focus;
    }
    (focus + 1..entries.len())
        .filter(|index| entries[*index].focusable)
        .nth(margin - 1)
        .unwrap_or(entries.len() - 1)
}

/// The Rows the entries from `from` to `to`, both included, take.
fn rows_between(entries: &[WindowEntry], from: usize, to: usize) -> usize {
    entries[from..=to].iter().map(|entry| entry.rows).sum()
}

/// The earliest entry a window of `capacity` Rows can open on while still
/// showing the whole of the entry at `last_shown`. An entry too tall for the
/// window at all is opened on regardless.
fn earliest_opening(entries: &[WindowEntry], last_shown: usize, capacity: usize) -> usize {
    let mut used = 0;
    let mut opening = last_shown;
    for (index, entry) in entries.iter().enumerate().take(last_shown + 1).rev() {
        used += entry.rows;
        if used > capacity {
            break;
        }
        opening = index;
    }
    opening
}

/// One past the last entry a window of `capacity` Rows opening on `first`
/// shows whole — and never short of `first` itself.
fn window_end(entries: &[WindowEntry], first: usize, capacity: usize) -> usize {
    let mut used = 0;
    let mut end = first;
    for entry in &entries[first..] {
        used += entry.rows;
        if used > capacity && end > first {
            break;
        }
        end += 1;
        if used >= capacity {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list of `len` single-Row entries, each one focus can stand on.
    fn rows(len: usize) -> Vec<WindowEntry> {
        vec![WindowEntry::ROW; len]
    }

    /// Walks focus from `from` through each of `steps` as the keys would,
    /// answering the window each step leaves drawn.
    fn walk(
        window: &ListWindow,
        entries: &[WindowEntry],
        capacity: usize,
        steps: impl IntoIterator<Item = usize>,
    ) -> Vec<Range<usize>> {
        steps
            .into_iter()
            .map(|focus| {
                window.reveal();
                window.settle(entries, capacity, Some(focus))
            })
            .collect()
    }

    /// A window opened on `focus` and settled once.
    fn opened_on(entries: &[WindowEntry], capacity: usize, focus: usize) -> ListWindow {
        let window = ListWindow::default();
        window.open();
        window.settle(entries, capacity, Some(focus));
        window
    }

    #[test]
    fn keying_down_holds_the_window_until_two_entries_are_left_below_focus() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 0);

        assert_eq!(
            walk(&window, &entries, 6, 1..=5),
            vec![0..6, 0..6, 0..6, 1..7, 2..8],
            "focus walks to the third-last Row shown before the list moves, then the list moves a Row a step"
        );
    }

    #[test]
    fn keying_back_up_holds_the_window_until_two_entries_are_left_above_focus() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 19);
        assert_eq!(window.first(), 14, "the list's foot is shown");

        assert_eq!(
            walk(&window, &entries, 6, (13..=18).rev()),
            vec![14..20, 14..20, 14..20, 13..19, 12..18, 11..17],
            "walking back up leaves the list standing until focus is third from the top"
        );
    }

    #[test]
    fn the_window_does_not_move_while_the_lists_end_already_shows() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 19);
        assert_eq!(
            walk(&window, &entries, 6, [18, 19]),
            vec![14..20, 14..20],
            "the foot of the list is already on show, so only focus moves"
        );

        let window = opened_on(&entries, 6, 0);
        assert_eq!(
            walk(&window, &entries, 6, [1, 0]),
            vec![0..6, 0..6],
            "and so is its head"
        );
    }

    #[test]
    fn a_window_too_short_for_two_either_side_keeps_what_it_can_spare_evenly() {
        let entries = rows(20);

        let window = opened_on(&entries, 3, 0);
        assert_eq!(
            walk(&window, &entries, 3, 1..=4),
            vec![0..3, 1..4, 2..5, 3..6],
            "three Rows keep one either side, so focus rides the middle"
        );

        let window = opened_on(&entries, 4, 0);
        assert_eq!(
            walk(&window, &entries, 4, 1..=4),
            vec![0..4, 0..4, 1..5, 2..6],
            "four Rows cannot spare two either side, so they keep one"
        );
        assert_eq!(
            walk(&window, &entries, 4, (1..=3).rev()),
            vec![2..6, 1..5, 0..4],
            "and keep it on the way back up"
        );

        let window = opened_on(&entries, 1, 0);
        assert_eq!(
            walk(&window, &entries, 1, 1..=2),
            vec![1..2, 2..3],
            "a single Row can spare nothing"
        );
    }

    #[test]
    fn the_margin_is_counted_in_entries_however_many_rows_each_takes() {
        // Five entries of two Rows fill a window of ten.
        let entries = vec![WindowEntry::focusable(2); 12];
        let window = opened_on(&entries, 10, 0);

        assert_eq!(
            walk(&window, &entries, 10, 1..=4),
            vec![0..5, 0..5, 1..6, 2..7],
            "two whole entries stay below focus, four Rows of them"
        );
    }

    #[test]
    fn entries_focus_cannot_stand_on_come_into_view_without_counting_towards_the_margin() {
        // Three Providers of two Models each, every one headed by its name.
        let entries = [
            WindowEntry::passive(1),
            WindowEntry::ROW,
            WindowEntry::ROW,
            WindowEntry::passive(1),
            WindowEntry::ROW,
            WindowEntry::ROW,
            WindowEntry::passive(1),
            WindowEntry::ROW,
            WindowEntry::ROW,
        ];
        let window = opened_on(&entries, 7, 1);

        assert_eq!(
            walk(&window, &entries, 7, [2, 4, 5]),
            vec![0..7, 1..8, 2..9],
            "two Models stay below focus, with the heading between them in view too"
        );
        assert_eq!(
            walk(&window, &entries, 7, [4, 2, 1]),
            vec![1..8, 0..7, 0..7],
            "and two above it on the way back, up to the heading the list opens on"
        );
    }

    #[test]
    fn a_page_of_the_keys_moves_the_window_only_as_far_as_the_margin_asks() {
        let entries = rows(40);
        let window = opened_on(&entries, 6, 0);

        assert_eq!(
            walk(&window, &entries, 6, [10, 20, 10]),
            vec![7..13, 17..23, 8..14],
            "paging lands focus two entries from the edge it moved towards"
        );
    }

    #[test]
    fn wrapping_past_either_end_lands_the_window_at_the_other() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 19);

        assert_eq!(walk(&window, &entries, 6, [0]), vec![0..6]);
        assert_eq!(walk(&window, &entries, 6, [19]), vec![14..20]);
    }

    #[test]
    fn a_list_opens_as_though_the_keys_had_walked_from_its_head_to_focus() {
        let entries = rows(20);

        let window = ListWindow::default();
        window.open();
        assert_eq!(
            window.settle(&entries, 6, Some(12)),
            9..15,
            "focus beyond the first page stands third from the foot"
        );

        window.open();
        assert_eq!(
            window.settle(&entries, 6, Some(2)),
            0..6,
            "and focus within it leaves the list at its head"
        );
    }

    #[test]
    fn a_list_still_loading_opens_on_focus_once_its_entries_land() {
        let window = ListWindow::default();
        window.open();
        assert_eq!(window.settle(&[], 6, None), 0..0);
        assert_eq!(window.settle(&rows(20), 6, None), 0..6);

        assert_eq!(
            window.settle(&rows(20), 6, Some(12)),
            9..15,
            "the opening waits for an entry focus stands on"
        );
    }

    #[test]
    fn focus_moved_by_anything_but_the_keys_leaves_the_window_standing() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 0);
        walk(&window, &entries, 6, [12]);
        assert_eq!(window.first(), 9);

        assert_eq!(
            window.settle(&entries, 6, Some(14)),
            9..15,
            "a pointer landing on the foot of the window moves nothing"
        );
        assert_eq!(
            window.settle(&entries, 6, Some(2)),
            9..15,
            "nor does focus carried out of view by the entries changing"
        );
        assert_eq!(
            walk(&window, &entries, 6, [3]),
            vec![1..7],
            "until the keys move it again"
        );
    }

    #[test]
    fn a_list_growing_shorter_holds_its_window_as_far_down_as_it_still_fills() {
        let window = opened_on(&rows(20), 6, 12);
        assert_eq!(window.first(), 9);

        assert_eq!(
            window.settle(&rows(16), 6, Some(12)),
            9..15,
            "the entries the window opened on survive, so it stays on them"
        );
        assert_eq!(
            window.settle(&rows(12), 6, Some(11)),
            6..12,
            "and gives way only as far as keeps blank Rows off its foot"
        );
        assert_eq!(window.settle(&rows(4), 6, Some(3)), 0..4);
    }

    #[test]
    fn a_window_growing_shorter_keeps_the_focus_it_was_showing_in_view() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 0);
        walk(&window, &entries, 6, [3]);

        assert_eq!(
            window.settle(&entries, 3, Some(3)),
            1..4,
            "the window gives up its head rather than the focus at its foot"
        );
        assert_eq!(
            window.settle(&entries, 6, Some(3)),
            1..7,
            "and growing again leaves it standing"
        );

        let window = opened_on(&entries, 6, 0);
        assert_eq!(
            window.settle(&entries, 3, Some(10)),
            0..3,
            "focus it was not showing stays out of view"
        );
    }

    #[test]
    fn the_reader_moving_the_window_is_where_it_holds() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 0);
        window.reveal();

        window.scroll_to(8);

        assert_eq!(
            window.settle(&entries, 6, Some(0)),
            8..14,
            "moving the window answers any reveal still waiting"
        );
    }

    #[test]
    fn the_pointer_takes_back_a_reveal_but_not_an_opening() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 0);
        window.reveal();

        window.hold();

        assert_eq!(
            window.settle(&entries, 6, Some(5)),
            0..6,
            "the pointer landing on the foot of the window leaves it standing"
        );

        window.open();
        window.hold();
        assert_eq!(
            window.settle(&entries, 6, Some(14)),
            11..17,
            "while a list opening still opens on focus"
        );
    }

    #[test]
    fn another_session_opening_carries_the_window_to_its_entry() {
        let entries = rows(20);
        let window = ListWindow::default();
        let open = OpenEntry::default();

        open.follow(&window, Some(&12));
        assert_eq!(
            window.settle(&entries, 6, Some(12)),
            9..15,
            "the first frame to draw the open entry carries the window to it"
        );

        open.follow(&window, Some(&12));
        assert_eq!(
            window.settle(&entries, 6, Some(17)),
            9..15,
            "the same Session standing open asks nothing"
        );

        open.follow(&window, Some(&3));
        assert_eq!(
            window.settle(&entries, 6, Some(3)),
            1..7,
            "another opening carries it as the keys would"
        );
    }

    #[test]
    fn a_session_opened_by_a_press_moves_no_window() {
        let entries = rows(20);
        let window = ListWindow::default();
        let open = OpenEntry::default();
        open.follow(&window, None);
        window.settle(&entries, 6, None);
        window.reveal();

        open.press();
        open.follow(&window, Some(&5));
        assert_eq!(
            window.settle(&entries, 6, Some(5)),
            0..6,
            "what the press opened, and any reveal it asked for, was already in view"
        );

        open.follow(&window, Some(&14));
        assert_eq!(
            window.settle(&entries, 6, Some(14)),
            11..17,
            "and the press is spent on the one frame"
        );
    }

    #[test]
    fn an_entry_taller_than_the_window_is_shown_cut_rather_than_not_at_all() {
        let entries = [
            WindowEntry::focusable(1),
            WindowEntry::focusable(3),
            WindowEntry::focusable(1),
        ];
        let window = opened_on(&entries, 2, 0);

        assert_eq!(walk(&window, &entries, 2, [1]), vec![1..2]);
        assert_eq!(walk(&window, &entries, 2, [2]), vec![2..3]);
    }

    #[test]
    fn a_window_with_no_rows_shows_nothing_and_forgets_nothing() {
        let entries = rows(20);
        let window = opened_on(&entries, 6, 12);
        window.reveal();

        assert_eq!(window.settle(&entries, 0, Some(0)), 0..0);
        assert_eq!(
            window.settle(&entries, 6, Some(0)),
            0..6,
            "the reveal waits for a window that can show something"
        );
    }

    #[test]
    fn the_furthest_a_window_opens_is_where_the_rest_of_the_list_fits_beneath() {
        let entries = [
            WindowEntry::focusable(1),
            WindowEntry::focusable(2),
            WindowEntry::focusable(2),
            WindowEntry::focusable(2),
        ];
        assert_eq!(furthest_opening(&entries, 7), 0, "all of it fits");
        assert_eq!(
            furthest_opening(&entries, 6),
            1,
            "the one-Row head alone spills"
        );
        assert_eq!(furthest_opening(&entries, 4), 2, "two entries fit whole");
        assert_eq!(
            furthest_opening(&entries, 3),
            3,
            "one entry fits, and its neighbour's Row is not cut"
        );
        assert_eq!(
            furthest_opening(&entries, 1),
            3,
            "a window shorter than the last entry still opens on it rather than past it"
        );
        assert_eq!(furthest_opening(&[], 3), 0);
    }

    #[test]
    fn a_window_shows_only_the_rows_it_has() {
        let window = ListWindow::default();
        window.open();

        assert_eq!(
            window
                .show((0..20).collect::<Vec<_>>(), 6, Some(12))
                .collect::<Vec<_>>(),
            (9..15).collect::<Vec<_>>()
        );
    }
}
