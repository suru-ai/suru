//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Server worked over the range the Overview tab shares with it, as a grid
//! of panels each reading its own part of one report.
//!
//! A panel is a variant of [`InsightsPanel`]: where it stands among the
//! others and in the order focus moves through them is [`InsightsPanel::ALL`];
//! its title, its height, and how it draws itself — what it says where the
//! range holds nothing for it among them — are its module's; and so are the
//! items it lets a reader select, by their place, which the panel commands
//! move among and select whichever panel holds them. Only the focused panel
//! holds a selection, so Esc clears that, then leaves the panel, then
//! closes the view.

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

pub(super) use view::render;

/// A panel of the Insights tab.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InsightsPanel {
    /// How long Turns took to Settle, and how they ended.
    Turns,
}

impl InsightsPanel {
    /// Every panel, in the order the grid lays them out, row by row across
    /// its two columns, and focus moves among them.
    pub(crate) const ALL: [Self; 1] = [Self::Turns];

    pub(crate) fn place(self) -> usize {
        Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .expect("every panel is laid out")
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Turns => "Turns",
        }
    }

    /// How many items `report` gives the panel for a reader to select among,
    /// in the order its keys move through them.
    fn items(self, report: &InsightsReport) -> usize {
        match self {
            Self::Turns => turns::items(&report.turns),
        }
    }
}

/// Where a selection within a panel moves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum ItemMove {
    /// The item before, or the last where none is selected.
    Previous,
    /// The item after, or the first where none is selected.
    Next,
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
    Report(&'a InsightsReport),
}

/// How a panel's items stand where it drew them, from its first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ItemRun {
    /// Side by side, a column apiece, as the Turns panel's bins are.
    Across,
}

/// Where the last frame drew each part of the tab a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    /// Each panel's place in the grid, borders included, in
    /// [`InsightsPanel::ALL`] order.
    panels: [Option<Rect>; InsightsPanel::ALL.len()],
    /// Where each panel drew the items a reader selects among, and how they
    /// stand, in the same order.
    items: [Option<(Rect, ItemRun)>; InsightsPanel::ALL.len()],
}

/// The tab's state: the reports it read, kept by range, and the panel the
/// keys act on. What it read does not outlive the view's closing.
#[derive(Clone, Debug, Default)]
pub(in crate::tui) struct InsightsView {
    /// The read it awaits: its request, the Outlook it asked, and the range
    /// it asked for.
    asked: Option<(Uuid, Outlook, MetricsRange)>,
    /// The last report read of each range, with the Outlook it was read
    /// from, so switching between tabs or ranges reads nothing again. Only a
    /// report read from the Outlook the view is turned toward is shown.
    read: HashMap<MetricsRange, (Outlook, InsightsReport)>,
    /// Why the last read of a range, from an Outlook, gave no report.
    refused: Option<(Outlook, MetricsRange, UsageMetricsReadRefusal)>,
    focused: Option<InsightsPanel>,
    /// The item selected within the focused panel, by its place among the
    /// panel's items.
    selected: Option<usize>,
    targets: Cell<Targets>,
}

impl InsightsView {
    /// The report kept for `range` from `origin`, where one is.
    fn report(&self, origin: &Outlook, range: MetricsRange) -> Option<&InsightsReport> {
        self.read
            .get(&range)
            .filter(|(read_from, _)| read_from == origin)
            .map(|(_, report)| report)
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
        self.asked = Some((request_id, origin, range));
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
            .is_some_and(|(asked, _, _)| *asked == request_id)
        {
            return;
        }
        let (_, origin, range) = self.asked.take().expect("the awaited read");
        match result {
            Ok(report) => {
                self.read.insert(range, (origin, report));
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
            .is_some_and(|(_, asked_of, asked)| asked_of == origin && *asked == range)
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
            .and_then(|report| panel.items(report).checked_sub(1))
        else {
            return;
        };
        self.selected = Some(match (to, self.selected) {
            (ItemMove::Previous, Some(item)) => item.saturating_sub(1).min(last),
            (ItemMove::Next, Some(item)) => (item + 1).min(last),
            (ItemMove::Previous, None) => last,
            (ItemMove::Next, None) => 0,
        });
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
        let held = self
            .report(origin, range)
            .is_some_and(|report| item < panel.items(report));
        if held {
            self.focus(panel);
            self.selected = Some(item);
        }
    }

    /// Forgets where the last frame drew anything, before the next draws.
    pub(in crate::tui) fn forget_targets(&self) {
        self.targets.set(Targets::default());
    }

    pub(in crate::tui) fn record_panel_target(&self, panel: InsightsPanel, area: Rect) {
        self.update_targets(|targets| targets.panels[panel.place()] = Some(area));
    }

    /// Records where `panel` drew its items, standing as `run` from the
    /// corner of `area`.
    pub(super) fn record_item_targets(&self, panel: InsightsPanel, area: Rect, run: ItemRun) {
        self.update_targets(|targets| targets.items[panel.place()] = Some((area, run)));
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
            .filter(|(area, _)| area.contains(position))
            .map(|(area, run)| match run {
                ItemRun::Across => usize::from(position.x - area.x),
            });
        Some(match item {
            Some(item) => InsightsTarget::Item { panel, item },
            None => InsightsTarget::Panel(panel),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{InsightsContent, InsightsView, MetricsRange, Outlook};
    use crate::protocol::{InsightsReport, SessionTimestamp};

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
