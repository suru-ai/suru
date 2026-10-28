//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Server worked over the range the Overview tab shares with it, as a grid
//! of panels each reading its own part of one report.
//!
//! A panel is a variant of [`InsightsPanel`]: where it stands among the
//! others and in the order focus moves through them is [`InsightsPanel::ALL`];
//! its title, its height, and how it draws itself — what it says where the
//! range holds nothing for it among them — are its module's; and what it
//! lets a reader select is a variant of [`PanelSelection`], which its keys,
//! clicks, and wheel invoke the semantic commands of. Only the focused panel
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
}

/// What a reader selected within the focused panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::tui) enum PanelSelection {
    /// A bin of the Turns panel's durations, by its place among them.
    TurnsBin(usize),
}

impl PanelSelection {
    fn panel(self) -> InsightsPanel {
        match self {
            Self::TurnsBin(_) => InsightsPanel::Turns,
        }
    }

    /// The Turns panel's bin selected, where this selects one.
    pub(super) fn turns_bin(self) -> Option<usize> {
        match self {
            Self::TurnsBin(bin) => Some(bin),
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
    /// The item at that place, or the last for a place past it.
    To(usize),
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
    /// A bin of the Turns panel's sparkline, by its place among them.
    TurnsBin(usize),
}

impl InsightsTarget {
    pub(in crate::tui) fn panel(self) -> InsightsPanel {
        match self {
            Self::Panel(panel) => panel,
            Self::TurnsBin(_) => InsightsPanel::Turns,
        }
    }
}

/// What the tab shows beneath the range bar.
pub(super) enum InsightsContent<'a> {
    Reading,
    Refused(&'a UsageMetricsReadRefusal),
    Report(&'a InsightsReport),
}

/// Where the last frame drew each part of the tab a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    /// Each panel's place in the grid, borders included, in
    /// [`InsightsPanel::ALL`] order.
    panels: [Option<Rect>; InsightsPanel::ALL.len()],
    /// The Turns panel's sparkline, a column to each bin.
    turns_bins: Option<Rect>,
}

/// The tab's state: the reports it read, kept by range, and the panel the
/// keys act on. What it read does not outlive the view's closing.
#[derive(Clone, Debug, Default)]
pub(in crate::tui) struct InsightsView {
    /// The read it awaits: its request, the Outlook it asked, and the range
    /// it asked for.
    asked: Option<(Uuid, Outlook, MetricsRange)>,
    /// The last report read of each range, with the Outlook it was read
    /// from, so switching between tabs or ranges reads nothing again.
    read: HashMap<MetricsRange, (Outlook, InsightsReport)>,
    /// Why the last read of a range gave no report.
    refused: Option<(MetricsRange, UsageMetricsReadRefusal)>,
    focused: Option<InsightsPanel>,
    /// What is selected within the focused panel.
    selection: Option<PanelSelection>,
    targets: Cell<Targets>,
}

impl InsightsView {
    /// Whether `range` lacks a report from `origin` with none on its way, so
    /// that showing it must read it.
    pub(in crate::tui) fn lacks(&self, origin: &Outlook, range: MetricsRange) -> bool {
        let kept = self
            .read
            .get(&range)
            .is_some_and(|(read_from, _)| read_from == origin);
        let awaited = self
            .asked
            .as_ref()
            .is_some_and(|(_, asked_of, asked)| asked_of == origin && *asked == range);
        !kept && !awaited
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
            Err(refusal) => self.refuse(range, refusal),
        }
    }

    /// Says why `range` has no fresh report, giving up any read awaited. A
    /// report already kept for it stays, with the refusal beside it.
    pub(in crate::tui) fn refuse(&mut self, range: MetricsRange, refusal: UsageMetricsReadRefusal) {
        self.asked = None;
        self.refused = Some((range, refusal));
    }

    /// Lets go of the read awaited, as the view closes.
    pub(in crate::tui) fn stop_awaiting(&mut self) {
        self.asked = None;
    }

    /// Whether a read of `range` is on its way.
    pub(in crate::tui) fn is_reading(&self, range: MetricsRange) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|(_, _, asked)| *asked == range)
    }

    fn refusal_of(&self, range: MetricsRange) -> Option<&UsageMetricsReadRefusal> {
        self.refused
            .as_ref()
            .filter(|(refused, _)| *refused == range)
            .map(|(_, refusal)| refusal)
    }

    pub(super) fn content(&self, range: MetricsRange) -> InsightsContent<'_> {
        match (self.read.get(&range), self.refusal_of(range)) {
            (Some((_, report)), _) => InsightsContent::Report(report),
            (None, Some(refusal)) => InsightsContent::Refused(refusal),
            (None, None) => InsightsContent::Reading,
        }
    }

    /// Why the report shown for `range` could not be read again, where it
    /// could not.
    pub(in crate::tui) fn stale_because(
        &self,
        range: MetricsRange,
    ) -> Option<&UsageMetricsReadRefusal> {
        self.read.get(&range).and_then(|_| self.refusal_of(range))
    }

    pub(in crate::tui) fn focused(&self) -> Option<InsightsPanel> {
        self.focused
    }

    /// Focuses `panel`, letting go of what another panel had selected.
    pub(in crate::tui) fn focus(&mut self, panel: InsightsPanel) {
        if self.focused != Some(panel) {
            self.selection = None;
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
        self.selection = None;
    }

    pub(in crate::tui) fn selection(&self) -> Option<PanelSelection> {
        self.selection
    }

    pub(in crate::tui) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Selects within `panel`, focusing it.
    fn select(&mut self, selection: PanelSelection) {
        self.focus(selection.panel());
        self.selection = Some(selection);
    }

    /// Moves the Turns panel's bin selection, focusing the panel, where the
    /// report shown for `range` has durations to select among.
    pub(in crate::tui) fn move_turns_bin(&mut self, range: MetricsRange, to: ItemMove) {
        let InsightsContent::Report(report) = self.content(range) else {
            return;
        };
        if report.turns.durations.is_none() {
            return;
        }
        let selected = self.selection.and_then(PanelSelection::turns_bin);
        self.select(PanelSelection::TurnsBin(turns::moved_bin(selected, to)));
    }

    /// Forgets where the last frame drew anything, before the next draws.
    pub(in crate::tui) fn forget_targets(&self) {
        self.targets.set(Targets::default());
    }

    pub(in crate::tui) fn record_panel_target(&self, panel: InsightsPanel, area: Rect) {
        self.update_targets(|targets| targets.panels[panel.place()] = Some(area));
    }

    pub(in crate::tui) fn record_turns_bins_target(&self, area: Rect) {
        self.update_targets(|targets| targets.turns_bins = Some(area));
    }

    fn update_targets(&self, update: impl FnOnce(&mut Targets)) {
        let mut targets = self.targets.get();
        update(&mut targets);
        self.targets.set(targets);
    }

    /// What the last frame drew at `position` that a pointer can act on.
    pub(in crate::tui) fn target_at(&self, position: Position) -> Option<InsightsTarget> {
        let targets = self.targets.get();
        if let Some(bins) = targets.turns_bins.filter(|bins| bins.contains(position)) {
            return Some(InsightsTarget::TurnsBin(usize::from(position.x - bins.x)));
        }
        InsightsPanel::ALL
            .into_iter()
            .find(|panel| targets.panels[panel.place()].is_some_and(|area| area.contains(position)))
            .map(InsightsTarget::Panel)
    }
}
