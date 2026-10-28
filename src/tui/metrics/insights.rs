//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Server worked over the range the Overview tab shares with it, as a grid
//! of panels each reading its own part of one report.
//!
//! A panel is a variant of [`InsightsPanel`]: where it stands among the
//! others and in the order focus moves through them is [`InsightsPanel::ALL`];
//! its title, its height, and how it draws itself — what it says where the
//! range holds nothing for it among them — are its module's; and so are the
//! items it lets a reader select, by their place, which the panel commands
//! move among and select whichever panel holds them. A panel's items stand
//! in rows of as many as [`InsightsPanel::across`] says, the first row
//! first, so ←/→ step through them in order and ↑/↓ move a row. Only the
//! focused panel holds a selection, so Esc clears that, then leaves the
//! panel, then closes the view.
//!
//! A panel with more rows than it has room for shows those it can, a
//! [`RowWindow`] onto them, and folds the rest into a last "(N more)" row:
//! the wheel scrolls it, and a selection moving past either end of the
//! window scrolls it along.
//!
//! A report is kept with its quarter-hours folded into the week of the zone
//! its range was cut in, once as it arrives rather than at every frame.

mod approvals;
mod compactions;
pub(super) mod heatmap;
pub(super) mod tools;
pub(super) mod turns;
mod view;

use std::{cell::Cell, collections::HashMap};

use jiff::tz::TimeZone;
use ratatui::layout::{Position, Rect};
use uuid::Uuid;

use crate::protocol::{InsightsQuery, InsightsReport, Outlook, SessionTimestamp};

use super::{
    UsageMetricsReadRefusal,
    calendar::{self, MetricsRange},
};
use heatmap::Heatmap;
pub(in crate::tui) use tools::ToolsSort;

pub(super) use view::render;

/// A panel of the Insights tab.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InsightsPanel {
    /// The Turns placed in each hour of the reader's week.
    WhenYouWork,
    /// How long Turns took to Settle, and how they ended.
    Turns,
    /// How often each Tool was used, and how many of those uses failed.
    Tools,
    /// How many Compactions there were, of each trigger and how many
    /// failed, and how much of the context they freed.
    Compactions,
    /// How many Approvals were asked, and how each was decided.
    Approvals,
}

impl InsightsPanel {
    /// Every panel, in the order [`Self::GRID`] places them row by row
    /// across its two columns, which is the order focus moves among them.
    pub(crate) const ALL: [Self; 5] = {
        let mut all = [Self::WhenYouWork; 5];
        let mut placed = 0;
        let mut row = 0;
        while row < Self::GRID.len() {
            let mut column = 0;
            while column < Self::GRID[row].len() {
                if let Some(panel) = Self::GRID[row][column] {
                    all[placed] = panel;
                    placed += 1;
                }
                column += 1;
            }
            row += 1;
        }
        assert!(placed == all.len(), "the grid places every panel");
        all
    };

    /// Where each panel stands in an interim grid of two columns, row by
    /// row, keeping to the neighbours the Insights mockup gives the panels
    /// built so far; the empty place is the Cache panel's, yet to be.
    pub(super) const GRID: [[Option<Self>; 2]; 3] = [
        [Some(Self::WhenYouWork), Some(Self::Tools)],
        [Some(Self::Turns), Some(Self::Compactions)],
        [None, Some(Self::Approvals)],
    ];

    pub(crate) fn place(self) -> usize {
        Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .expect("every panel is laid out")
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::WhenYouWork => "When you work (Turns by hour)",
            Self::Turns => "Turns",
            Self::Tools => "Tools used",
            Self::Compactions => "Compactions",
            Self::Approvals => "Approvals",
        }
    }

    /// How many items `kept` gives the panel for a reader to select among,
    /// in the order its keys move through them.
    fn items(self, kept: &KeptReport) -> usize {
        match self {
            Self::WhenYouWork => heatmap::items(&kept.heatmap),
            Self::Turns => turns::items(&kept.report.turns),
            Self::Tools => tools::items(&kept.report.tools),
            Self::Compactions | Self::Approvals => 0,
        }
    }

    /// How many of the panel's items stand in each row.
    fn across(self) -> usize {
        match self {
            Self::WhenYouWork => heatmap::HOURS,
            Self::Turns => crate::protocol::TURN_DURATION_BINS,
            Self::Tools | Self::Compactions | Self::Approvals => 1,
        }
    }

    /// How many lines the panel has for its rows of items, where it may
    /// have more than fit, so that it scrolls; none where all stand in view.
    fn room(self) -> Option<usize> {
        match self {
            Self::WhenYouWork | Self::Turns | Self::Compactions | Self::Approvals => None,
            Self::Tools => Some(tools::SHOWN),
        }
    }
}

/// Which of a panel's rows stand in view, of more than its lines can hold:
/// those from `first` on, and where any lie past them, a last line folding
/// those away as "(N more)". Scrolled to the end, the last rows fill every
/// line, with nothing left to fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RowWindow {
    pub(super) first: usize,
    pub(super) shown: usize,
    pub(super) folded: usize,
}

impl RowWindow {
    /// The window onto `total` rows in `room` lines, scrolled `offset`
    /// rows down, and no further than the last rows fill the lines.
    pub(super) fn new(total: usize, room: usize, offset: usize) -> Self {
        if total <= room {
            return Self {
                first: 0,
                shown: total,
                folded: 0,
            };
        }
        let first = offset.min(total - room);
        if first == total - room {
            return Self {
                first,
                shown: room,
                folded: 0,
            };
        }
        let shown = room.saturating_sub(1);
        Self {
            first,
            shown,
            folded: total - first - shown,
        }
    }

    /// How far down the window onto `total` rows in `room` lines,
    /// scrolled `offset` rows down, is scrolled once it shows the row
    /// `row`: as far as it was where it shows it already, and otherwise
    /// only as far as brings it into view.
    fn showing(total: usize, room: usize, offset: usize, row: usize) -> usize {
        let window = Self::new(total, room, offset);
        if row < window.first {
            row
        } else if row >= window.first + window.shown {
            // The row stands last above the fold, or last of all.
            (row + 2)
                .saturating_sub(room)
                .min(total.saturating_sub(room))
        } else {
            window.first
        }
    }
}

/// Which way a panel's rows scroll.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum RowScroll {
    /// A row back toward the first.
    Up,
    /// A row on toward the last.
    Down,
}

/// Where a selection within a panel moves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum ItemMove {
    /// The item before, or the last where none is selected.
    Previous,
    /// The item after, or the first where none is selected.
    Next,
    /// The item a row above, or the last where none is selected.
    Up,
    /// The item a row below, or the first where none is selected.
    Down,
}

/// Where focus moves among the panels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum PanelMove {
    /// The panel before, the first turning round to the last, or the last
    /// where none is focused.
    Previous,
    /// The panel after, the last turning round to the first, or the first
    /// where none is focused.
    Next,
}

/// A part of the Insights tab a pointer can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsightsTarget {
    /// A panel, away from anything within it a pointer selects.
    Panel(InsightsPanel),
    /// An item of a panel a reader can select, by its place among them.
    Item { panel: InsightsPanel, item: usize },
}

impl InsightsTarget {
    pub(in crate::tui) fn panel(self) -> InsightsPanel {
        match self {
            Self::Panel(panel) | Self::Item { panel, .. } => panel,
        }
    }
}

/// What the tab shows beneath the range bar.
pub(super) enum InsightsContent<'a> {
    Reading,
    Refused(&'a UsageMetricsReadRefusal),
    Report(&'a KeptReport),
}

/// A report kept for a range: the Outlook it was read from, and its
/// quarter-hours folded into the week of the zone the range was cut in.
#[derive(Clone, Debug)]
pub(super) struct KeptReport {
    origin: Outlook,
    report: InsightsReport,
    heatmap: Heatmap,
}

/// Where the last frame drew each part of the tab a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    /// Each panel's place in the grid, borders included, in
    /// [`InsightsPanel::ALL`] order.
    panels: [Option<Rect>; InsightsPanel::ALL.len()],
    /// Where each panel drew the items a reader selects among, how many
    /// columns each takes, and the place of the first it drew, in the same
    /// order. They stand in rows a line apiece, as many to a row as
    /// [`InsightsPanel::across`] says: the Turns panel's bins a column
    /// apiece in one row, the heatmap's hours two apiece in a row for each
    /// day, and the Tools panel's Tools a row apiece from the first in view.
    items: [Option<(Rect, u16, usize)>; InsightsPanel::ALL.len()],
}

/// The tab's state: the reports it read, kept by range, and the panel the
/// keys act on. What it read does not outlive the view's closing.
#[derive(Clone, Debug, Default)]
pub(in crate::tui) struct InsightsView {
    /// The read it awaits: its request, the Outlook it asked, the range it
    /// asked for, and the zone that range was cut in.
    asked: Option<(Uuid, Outlook, MetricsRange, TimeZone)>,
    /// The last report read of each range, so switching between tabs or
    /// ranges reads nothing again. Only a report read from the Outlook the
    /// view is turned toward is shown.
    read: HashMap<MetricsRange, KeptReport>,
    /// Why the last read of a range, from an Outlook, gave no report.
    refused: Option<(Outlook, MetricsRange, UsageMetricsReadRefusal)>,
    focused: Option<InsightsPanel>,
    /// The item selected within the focused panel, by its place among the
    /// panel's items.
    selected: Option<usize>,
    /// How many rows down each panel is scrolled, in
    /// [`InsightsPanel::ALL`] order: always none for a panel whose rows all
    /// stand in view.
    scrolled: [usize; InsightsPanel::ALL.len()],
    tools_sort: ToolsSort,
    targets: Cell<Targets>,
}

impl InsightsView {
    /// The report kept for `range` from `origin`, where one is.
    fn report(&self, origin: &Outlook, range: MetricsRange) -> Option<&KeptReport> {
        self.read.get(&range).filter(|kept| kept.origin == *origin)
    }

    /// Whether `range` lacks a report from `origin` with none on its way, so
    /// that showing it must read it. A range whose read was refused lacks
    /// one, so showing it again reads it again.
    pub(in crate::tui) fn lacks(&self, origin: &Outlook, range: MetricsRange) -> bool {
        self.report(origin, range).is_none() && !self.is_reading(origin, range)
    }

    /// The query `origin` is asked for `range` at `now` in `zone`, which the
    /// tab now awaits the answer to. A moment the calendar cannot place is
    /// refused in the tab instead.
    pub(in crate::tui) fn ask(
        &mut self,
        origin: Outlook,
        range: MetricsRange,
        now: SessionTimestamp,
        zone: &TimeZone,
    ) -> Option<(Uuid, InsightsQuery)> {
        let Some(interval) = calendar::span(range, now, zone) else {
            self.refuse(
                origin,
                range,
                UsageMetricsReadRefusal::Failed(
                    "The clock reads a moment the calendar cannot place".to_owned(),
                ),
            );
            return None;
        };
        let request_id = Uuid::new_v4();
        self.asked = Some((request_id, origin, range, zone.clone()));
        self.refused = None;
        Some((request_id, InsightsQuery { interval }))
    }

    /// Lets go of every report kept but `range`'s, as a refresh of `range`
    /// does: the others would otherwise show older figures than it beside
    /// nothing saying so, and are read again when next shown. `range`'s own
    /// stays until the refresh replaces it.
    pub(in crate::tui) fn forget_all_but(&mut self, range: MetricsRange) {
        self.read.retain(|kept, _| *kept == range);
    }

    /// Takes the Server's answer, unless the tab has since closed or asked
    /// again, keeping a report under the range it was asked for.
    pub(in crate::tui) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<InsightsReport, UsageMetricsReadRefusal>,
    ) {
        if !self
            .asked
            .as_ref()
            .is_some_and(|(asked, _, _, _)| *asked == request_id)
        {
            return;
        }
        let (_, origin, range, zone) = self.asked.take().expect("the awaited read");
        match result {
            Ok(report) => {
                let heatmap = Heatmap::fold(&report.quarter_hours, &zone);
                let kept = KeptReport {
                    origin,
                    report,
                    heatmap,
                };
                self.read.insert(range, kept);
            }
            Err(refusal) => self.refuse(origin, range, refusal),
        }
    }

    /// Says why `range` has no fresh report from `origin`, giving up any
    /// read awaited. A report already kept for it stays, with the refusal
    /// beside it.
    pub(in crate::tui) fn refuse(
        &mut self,
        origin: Outlook,
        range: MetricsRange,
        refusal: UsageMetricsReadRefusal,
    ) {
        self.asked = None;
        self.refused = Some((origin, range, refusal));
    }

    /// Lets go of the read awaited, as the view closes.
    pub(in crate::tui) fn stop_awaiting(&mut self) {
        self.asked = None;
    }

    /// Whether a read of `range` from `origin` is on its way.
    pub(in crate::tui) fn is_reading(&self, origin: &Outlook, range: MetricsRange) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|(_, asked_of, asked, _)| asked_of == origin && *asked == range)
    }

    fn refusal_of(
        &self,
        origin: &Outlook,
        range: MetricsRange,
    ) -> Option<&UsageMetricsReadRefusal> {
        self.refused
            .as_ref()
            .filter(|(refused_by, refused, _)| refused_by == origin && *refused == range)
            .map(|(_, _, refusal)| refusal)
    }

    /// What the tab shows for `range`, read from `origin`.
    pub(super) fn content(&self, origin: &Outlook, range: MetricsRange) -> InsightsContent<'_> {
        match (self.report(origin, range), self.refusal_of(origin, range)) {
            (Some(report), _) => InsightsContent::Report(report),
            (None, Some(refusal)) => InsightsContent::Refused(refusal),
            (None, None) => InsightsContent::Reading,
        }
    }

    /// Why the report shown for `range` from `origin` could not be read
    /// again, where it could not.
    pub(in crate::tui) fn stale_because(
        &self,
        origin: &Outlook,
        range: MetricsRange,
    ) -> Option<&UsageMetricsReadRefusal> {
        self.report(origin, range)
            .and_then(|_| self.refusal_of(origin, range))
    }

    pub(in crate::tui) fn focused(&self) -> Option<InsightsPanel> {
        self.focused
    }

    /// Focuses `panel`, letting go of what another panel had selected.
    pub(in crate::tui) fn focus(&mut self, panel: InsightsPanel) {
        if self.focused != Some(panel) {
            self.selected = None;
        }
        self.focused = Some(panel);
    }

    pub(in crate::tui) fn move_focus(&mut self, to: PanelMove) {
        let count = InsightsPanel::ALL.len();
        let place = match (to, self.focused) {
            (PanelMove::Next, Some(focused)) => (focused.place() + 1) % count,
            (PanelMove::Previous, Some(focused)) => (focused.place() + count - 1) % count,
            (PanelMove::Next, None) => 0,
            (PanelMove::Previous, None) => count - 1,
        };
        self.focus(InsightsPanel::ALL[place]);
    }

    /// Leaves the focused panel, and what it had selected.
    pub(in crate::tui) fn clear_focus(&mut self) {
        self.focused = None;
        self.selected = None;
    }

    /// The item selected within the focused panel, by its place among them.
    pub(in crate::tui) fn selected_item(&self) -> Option<usize> {
        self.selected
    }

    pub(in crate::tui) fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Lets go of what the view showed of the range before: the item
    /// selected and how far each panel was scrolled stand for its figures.
    pub(in crate::tui) fn leave_range(&mut self) {
        self.selected = None;
        self.scrolled = Default::default();
    }

    /// How far down `panel`'s rows are scrolled.
    fn scrolled(&self, panel: InsightsPanel) -> usize {
        self.scrolled[panel.place()]
    }

    pub(in crate::tui) fn tools_sort(&self) -> ToolsSort {
        self.tools_sort
    }

    /// Sorts the Tools panel the other way, scrolled back to its first row
    /// and letting go of a Tool selected, whose place the order changes.
    pub(in crate::tui) fn toggle_tools_sort(&mut self) {
        self.tools_sort = self.tools_sort.toggled();
        self.scrolled[InsightsPanel::Tools.place()] = 0;
        if self.focused == Some(InsightsPanel::Tools) {
            self.selected = None;
        }
    }

    /// Scrolls `panel`'s rows in the report shown for `range` from `origin`
    /// a row `to`, stopping at either end, whether or not it is focused. A
    /// panel whose rows all stand in view does not scroll.
    pub(in crate::tui) fn scroll(
        &mut self,
        origin: &Outlook,
        range: MetricsRange,
        panel: InsightsPanel,
        to: RowScroll,
    ) {
        let (Some(room), Some(kept)) = (panel.room(), self.report(origin, range)) else {
            return;
        };
        let rows = panel.items(kept).div_ceil(panel.across());
        let scrolled = self.scrolled(panel);
        let window = RowWindow::new(rows, room, scrolled);
        self.scrolled[panel.place()] = match to {
            RowScroll::Up => window.first.saturating_sub(1),
            RowScroll::Down => RowWindow::new(rows, room, window.first + 1).first,
        };
    }

    /// Scrolls the focused panel just far enough to show the row of the
    /// item selected in it, of `items`.
    fn follow_selection(&mut self, items: usize) {
        let (Some(panel), Some(item)) = (self.focused, self.selected) else {
            return;
        };
        let Some(room) = panel.room() else {
            return;
        };
        let across = panel.across();
        let rows = items.div_ceil(across);
        let place = panel.place();
        self.scrolled[place] = RowWindow::showing(rows, room, self.scrolled[place], item / across);
    }

    /// Moves the selection among the focused panel's items in the report
    /// shown for `range` from `origin`, stopping at the first and last.
    /// With no panel focused, or none of its items to select, it does
    /// nothing.
    pub(in crate::tui) fn move_item(
        &mut self,
        origin: &Outlook,
        range: MetricsRange,
        to: ItemMove,
    ) {
        let Some(panel) = self.focused else {
            return;
        };
        let Some(last) = self
            .report(origin, range)
            .and_then(|kept| panel.items(kept).checked_sub(1))
        else {
            return;
        };
        let across = panel.across();
        let item = match (to, self.selected) {
            (ItemMove::Previous, Some(item)) => item.saturating_sub(1),
            (ItemMove::Next, Some(item)) => item + 1,
            // A row up from the first row, or down from the last, stays put.
            (ItemMove::Up, Some(item)) => item.checked_sub(across).unwrap_or(item),
            (ItemMove::Down, Some(item)) if item + across <= last => item + across,
            (ItemMove::Down, Some(item)) => item,
            (ItemMove::Previous | ItemMove::Up, None) => last,
            (ItemMove::Next | ItemMove::Down, None) => 0,
        };
        self.selected = Some(item.min(last));
        self.follow_selection(last + 1);
    }

    /// Selects `panel`'s `item` in the report shown for `range` from
    /// `origin`, focusing the panel, where it has that item.
    pub(in crate::tui) fn select_item(
        &mut self,
        origin: &Outlook,
        range: MetricsRange,
        panel: InsightsPanel,
        item: usize,
    ) {
        let items = self
            .report(origin, range)
            .map_or(0, |kept| panel.items(kept));
        if item < items {
            self.focus(panel);
            self.selected = Some(item);
            self.follow_selection(items);
        }
    }

    /// Forgets where the last frame drew anything, before the next draws.
    pub(in crate::tui) fn forget_targets(&self) {
        self.targets.set(Targets::default());
    }

    pub(in crate::tui) fn record_panel_target(&self, panel: InsightsPanel, area: Rect) {
        self.update_targets(|targets| targets.panels[panel.place()] = Some(area));
    }

    /// Records where `panel` drew its items, from the corner of `area`,
    /// each `width` columns wide, the first there the item at `first`.
    pub(super) fn record_item_targets(
        &self,
        panel: InsightsPanel,
        area: Rect,
        width: u16,
        first: usize,
    ) {
        self.update_targets(|targets| targets.items[panel.place()] = Some((area, width, first)));
    }

    fn update_targets(&self, update: impl FnOnce(&mut Targets)) {
        let mut targets = self.targets.get();
        update(&mut targets);
        self.targets.set(targets);
    }

    /// What the last frame drew at `position` that a pointer can act on.
    pub(in crate::tui) fn target_at(&self, position: Position) -> Option<InsightsTarget> {
        let targets = self.targets.get();
        let panel = InsightsPanel::ALL.into_iter().find(|panel| {
            targets.panels[panel.place()].is_some_and(|area| area.contains(position))
        })?;
        let item = targets.items[panel.place()]
            .filter(|(area, _, _)| area.contains(position))
            .map(|(area, width, first)| {
                let row = usize::from(position.y - area.y);
                let column = usize::from((position.x - area.x) / width.max(1));
                first + row * panel.across() + column
            });
        Some(match item {
            Some(item) => InsightsTarget::Item { panel, item },
            None => InsightsTarget::Panel(panel),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{InsightsContent, InsightsPanel, InsightsView, MetricsRange, Outlook};
    use crate::protocol::{InsightsReport, SessionTimestamp};

    /// The grid places each panel once, so each has a place of its own.
    #[test]
    fn the_grid_places_every_panel_once() {
        for (place, panel) in InsightsPanel::ALL.into_iter().enumerate() {
            assert_eq!(panel.place(), place, "{panel:?} is placed once");
        }
    }

    /// A report read from one Server is never shown for another, which
    /// lacks one of its own until it is read.
    #[test]
    fn a_report_shows_only_for_the_outlook_it_was_read_from() {
        let mut view = InsightsView::default();
        let range = MetricsRange::ThirtyDays;
        let (request_id, _) = view
            .ask(
                Outlook::Local,
                range,
                SessionTimestamp(1_800_000_000_000),
                &jiff::tz::TimeZone::UTC,
            )
            .expect("a placeable moment");
        view.receive(request_id, Ok(InsightsReport::default()));
        let studio = Outlook::Remote("studio".to_owned());

        assert!(matches!(
            view.content(&Outlook::Local, range),
            InsightsContent::Report(_)
        ));
        assert!(matches!(
            view.content(&studio, range),
            InsightsContent::Reading
        ));
        assert!(!view.lacks(&Outlook::Local, range));
        assert!(view.lacks(&studio, range));
    }
}
