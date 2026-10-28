//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Server worked over the range the Overview tab shares with it, as columns
//! of panels each reading its own part of one report.
//!
//! A panel is a variant of [`InsightsPanel`]: where it stands among the
//! others is [`InsightsPanel::LAYOUT`], and the order focus moves through
//! them, the order they are read in there, [`InsightsPanel::ALL`];
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
//! Wherever the panels run past the foot of the frame — stacked in one
//! column below the mockup's width, or a short frame of the grid — the
//! body scrolls: PageUp/PageDown a page, the wheel a step anywhere but
//! over a panel's own rows, which it scrolls first and carries on past
//! once they end, and focus moving to a panel scrolls it into view.
//!
//! A report is kept with its quarter-hours folded into the week of the zone
//! its range was cut in, once as it arrives rather than at every frame.

mod approvals;
mod cache;
mod compactions;
mod delegation;
pub(super) mod heatmap;
pub(super) mod tools;
pub(super) mod turns;
mod view;
mod workspaces;

use std::{cell::Cell, collections::HashMap};

use jiff::tz::TimeZone;
use ratatui::layout::{Position, Rect};
use uuid::Uuid;

use crate::protocol::{
    InsightsQuery, InsightsReport, Outlook, SessionTimestamp, UsageMetricsBreakdown,
    UsageMetricsKey, UsageMetricsQuery, UsageMetricsReport,
};

use super::{
    UsageMetricsReadRefusal,
    calendar::{self, MetricsRange},
};
use heatmap::Heatmap;
pub(in crate::tui) use tools::ToolsSort;

pub(super) use view::render;
use workspaces::WorkspaceRow;

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
    /// How much of the input was read from cache, and an estimate of
    /// what that saved.
    Cache,
    /// How many Approvals were asked, and how each was decided.
    Approvals,
    /// How Cost divides between the user's own Sessions and the work
    /// delegated to Subagents, Sidekicks, and Subsessions.
    Delegation,
    /// The Workspaces the range's work was done in, ranked by Cost, each
    /// opening its trend on Overview.
    Workspaces,
}

/// A part of the tab's layout: a panel, or columns standing side by side
/// within the width it is given.
#[derive(Clone, Copy, Debug)]
pub(super) enum Place {
    Panel(InsightsPanel),
    Beside(&'static [Column]),
}

/// A column of the layout: its places stacked top to bottom, each beneath
/// the last whatever stands beside it, its share of the width it stands
/// in, by weight among the columns beside it, and the narrowest it stands
/// beside them in: narrower, the columns stack instead.
#[derive(Clone, Copy, Debug)]
pub(super) struct Column {
    pub(super) weight: u16,
    pub(super) narrowest: u16,
    pub(super) places: &'static [Place],
}

impl InsightsPanel {
    /// Where each panel stands, as the Insights mockup lays them out: When
    /// you work at the top left, Turns then Cache in a column beneath it
    /// beside Compactions then Approvals, and Workspaces, Tools, then
    /// Delegation in the right-hand column. The weights share 120 columns
    /// as near as the panels' figures allow to the mockup's shares: When
    /// you work its hours two columns apiece, the small panels their
    /// briefest figures, and Delegation its bars beside their Costs. Each
    /// column stacks its own panels, so a short one never waits on its
    /// neighbour's height.
    ///
    /// The left-hand column stands beside the right from 50 columns, as
    /// wide as the mockup's own frame of 100 makes it: narrower, the two
    /// would cut short the hours of When you work and the names Workspaces
    /// and Tools give, so every panel stacks in one column as wide as the
    /// frame, which scrolls.
    pub(super) const LAYOUT: Place = Place::Beside(&[
        Column {
            weight: 61,
            narrowest: 50,
            places: &[
                Place::Panel(Self::WhenYouWork),
                Place::Beside(&[
                    Column {
                        weight: 29,
                        narrowest: view::BESIDE_WIDTH,
                        places: &[Place::Panel(Self::Turns), Place::Panel(Self::Cache)],
                    },
                    Column {
                        weight: 30,
                        narrowest: view::BESIDE_WIDTH,
                        places: &[
                            Place::Panel(Self::Compactions),
                            Place::Panel(Self::Approvals),
                        ],
                    },
                ]),
            ],
        },
        Column {
            weight: 53,
            narrowest: view::BESIDE_WIDTH,
            places: &[
                Place::Panel(Self::Workspaces),
                Place::Panel(Self::Tools),
                Place::Panel(Self::Delegation),
            ],
        },
    ]);

    /// How many panels there are, every one of which [`Self::LAYOUT`]
    /// places once.
    pub(crate) const COUNT: usize = 8;

    /// Every panel, in the order [`Self::LAYOUT`] reads them — down each
    /// column, the left first, columns standing beside one another read
    /// before the place beneath them — which is the order focus moves
    /// among them.
    pub(crate) const ALL: [Self; Self::COUNT] = {
        let mut all = [Self::WhenYouWork; Self::COUNT];
        let placed = Self::gather(&Self::LAYOUT, &mut all, 0);
        assert!(placed == all.len(), "the layout places every panel");
        all
    };

    /// Places `place`'s panels in `all` in reading order from `placed` on,
    /// answering how many are placed then.
    const fn gather(place: &Place, all: &mut [Self; Self::COUNT], placed: usize) -> usize {
        match place {
            Place::Panel(panel) => {
                all[placed] = *panel;
                placed + 1
            }
            Place::Beside(columns) => {
                let mut placed = placed;
                let mut column = 0;
                while column < columns.len() {
                    let places = columns[column].places;
                    let mut at = 0;
                    while at < places.len() {
                        placed = Self::gather(&places[at], all, placed);
                        at += 1;
                    }
                    column += 1;
                }
                placed
            }
        }
    }

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
            Self::Cache => "Cache",
            Self::Approvals => "Approvals",
            Self::Delegation => "Delegation (Cost)",
            Self::Workspaces => "Workspaces by Cost",
        }
    }

    /// How many items `kept` and `workspaces` give the panel for a reader
    /// to select among, in the order its keys move through them.
    fn items(self, kept: &KeptReport, workspaces: &[WorkspaceRow]) -> usize {
        match self {
            Self::WhenYouWork => heatmap::items(&kept.heatmap),
            Self::Turns => turns::items(&kept.report.turns),
            Self::Tools => tools::items(&kept.report.tools),
            Self::Workspaces => workspaces::items(workspaces),
            Self::Compactions | Self::Cache | Self::Approvals | Self::Delegation => 0,
        }
    }

    /// How many of the panel's items stand in each row.
    fn across(self) -> usize {
        match self {
            Self::WhenYouWork => heatmap::HOURS,
            Self::Turns => crate::protocol::TURN_DURATION_BINS,
            Self::Tools
            | Self::Workspaces
            | Self::Compactions
            | Self::Cache
            | Self::Approvals
            | Self::Delegation => 1,
        }
    }

    /// How many lines the panel has for its rows of items, where it may
    /// have more than fit, so that it scrolls; none where all stand in view.
    fn room(self) -> Option<usize> {
        match self {
            Self::WhenYouWork
            | Self::Turns
            | Self::Compactions
            | Self::Cache
            | Self::Approvals
            | Self::Delegation => None,
            Self::Tools => Some(tools::SHOWN),
            Self::Workspaces => Some(workspaces::SHOWN),
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
    /// The panels' body, between and beside them, which the wheel scrolls.
    Body,
    /// A panel, away from anything within it a pointer selects.
    Panel(InsightsPanel),
    /// An item of a panel a reader can select, by its place among them.
    Item { panel: InsightsPanel, item: usize },
}

impl InsightsTarget {
    /// The panel the target stands in: none for the body between them.
    pub(in crate::tui) fn panel(self) -> Option<InsightsPanel> {
        match self {
            Self::Panel(panel) | Self::Item { panel, .. } => Some(panel),
            Self::Body => None,
        }
    }
}

/// The reads showing the tab asks for together: its own, and the
/// one-bucket Usage Metrics read by Workspace over the same interval, which
/// the Workspaces panel ranks — no read of its own, since the metrics read
/// already answers it.
pub(in crate::tui) struct InsightsAsk {
    pub(in crate::tui) request_id: Uuid,
    pub(in crate::tui) query: InsightsQuery,
    pub(in crate::tui) workspaces_request_id: Uuid,
    pub(in crate::tui) workspaces: UsageMetricsQuery,
}

/// What the Workspaces panel shows: its rows, which may be none, or what
/// stands in for them while their read is on its way or where it was
/// refused.
pub(super) enum WorkspacesContent<'a> {
    Reading,
    Refused(&'a UsageMetricsReadRefusal),
    Rows(&'a [WorkspaceRow]),
}

/// What the tab shows beneath the range bar.
pub(super) enum InsightsContent<'a> {
    Reading,
    Refused(&'a UsageMetricsReadRefusal),
    Report(&'a KeptReport),
}

/// A report kept for a range, its quarter-hours folded into the week of the
/// zone the range was cut in.
#[derive(Clone, Debug)]
pub(super) struct KeptReport {
    report: InsightsReport,
    heatmap: Heatmap,
}

/// One of the reads the tab makes of each range it shows: the read awaited,
/// what the last answer for each range kept, with the Outlook it came from,
/// and why the last read was refused. `C` is what the read awaited needs
/// to keep its answer by: the zone its range was cut in, for the insights.
#[derive(Clone, Debug)]
struct RangeReads<T, C = ()> {
    asked: Option<AskedRead<C>>,
    /// The last answer kept for each range, so switching between tabs or
    /// ranges reads nothing again. Only what was read from the Outlook the
    /// view is turned toward is shown.
    kept: HashMap<MetricsRange, (Outlook, T)>,
    refused: Option<(Outlook, MetricsRange, UsageMetricsReadRefusal)>,
}

/// A read awaited: its request, the Outlook it asked, the range it asked
/// for, and what its answer is kept by.
#[derive(Clone, Debug)]
struct AskedRead<C> {
    request_id: Uuid,
    origin: Outlook,
    range: MetricsRange,
    context: C,
}

impl<T, C> Default for RangeReads<T, C> {
    fn default() -> Self {
        Self {
            asked: None,
            kept: HashMap::new(),
            refused: None,
        }
    }
}

impl<T, C> RangeReads<T, C> {
    /// What is kept for `range` from `origin`, where anything is.
    fn kept(&self, origin: &Outlook, range: MetricsRange) -> Option<&T> {
        self.kept
            .get(&range)
            .filter(|(read_from, _)| read_from == origin)
            .map(|(_, kept)| kept)
    }

    /// Whether the read of `range` from `origin` is on its way.
    fn is_reading(&self, origin: &Outlook, range: MetricsRange) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|asked| asked.origin == *origin && asked.range == range)
    }

    /// Why the last read of `range` from `origin` was refused, where it was.
    fn refusal(&self, origin: &Outlook, range: MetricsRange) -> Option<&UsageMetricsReadRefusal> {
        self.refused
            .as_ref()
            .filter(|(refused_by, refused, _)| refused_by == origin && *refused == range)
            .map(|(_, _, refusal)| refusal)
    }

    /// Awaits a read of `range` from `origin`, answering its request.
    fn ask(&mut self, origin: Outlook, range: MetricsRange, context: C) -> Uuid {
        let request_id = Uuid::new_v4();
        self.asked = Some(AskedRead {
            request_id,
            origin,
            range,
            context,
        });
        self.refused = None;
        request_id
    }

    /// Whether the read answering under `request_id` is awaited.
    fn awaits(&self, request_id: Uuid) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|asked| asked.request_id == request_id)
    }

    /// Takes the answer under `request_id`, unless it is no longer awaited,
    /// keeping what `keep` makes of it under the range it was asked for.
    fn receive<A>(
        &mut self,
        request_id: Uuid,
        result: Result<A, UsageMetricsReadRefusal>,
        keep: impl FnOnce(A, C) -> T,
    ) {
        if !self.awaits(request_id) {
            return;
        }
        let asked = self.asked.take().expect("the awaited read");
        match result {
            Ok(answer) => {
                let kept = keep(answer, asked.context);
                self.kept.insert(asked.range, (asked.origin, kept));
            }
            Err(refusal) => self.refuse(asked.origin, asked.range, refusal),
        }
    }

    /// Says why `range` has nothing fresh from `origin`, giving up the read
    /// awaited. What is already kept for it stays, the refusal beside it.
    fn refuse(&mut self, origin: Outlook, range: MetricsRange, refusal: UsageMetricsReadRefusal) {
        self.asked = None;
        self.refused = Some((origin, range, refusal));
    }

    fn stop_awaiting(&mut self) {
        self.asked = None;
    }

    /// Lets go of what is kept for every range but `range`.
    fn forget_all_but(&mut self, range: MetricsRange) {
        self.kept.retain(|kept, _| *kept == range);
    }
}

/// Where the last frame drew each part of the tab a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    /// Each panel's place in the layout, borders included, in
    /// [`InsightsPanel::ALL`] order.
    panels: [Option<Rect>; InsightsPanel::ALL.len()],
    /// Where each panel drew the items a reader selects among, how many
    /// columns each takes, and the place of the first it drew, in the same
    /// order. They stand in rows a line apiece, as many to a row as
    /// [`InsightsPanel::across`] says: the Turns panel's bins a column
    /// apiece in one row, the heatmap's hours two apiece in a row for each
    /// day, and the Tools panel's Tools a row apiece from the first in view.
    items: [Option<(Rect, u16, usize)>; InsightsPanel::ALL.len()],
    /// The part of the body the panels were shown through.
    body: Option<Rect>,
    /// Where the panels were laid out, before any was scrolled.
    laid: Option<Laid>,
}

/// Where the last frame laid the panels out in the body, from its top
/// before any scroll: the rows each took, how many they all took, and how
/// many of those the body showed at once.
#[derive(Clone, Copy, Debug)]
struct Laid {
    /// Each panel's first row and how many it took, in
    /// [`InsightsPanel::ALL`] order.
    panels: [Option<(u16, u16)>; InsightsPanel::ALL.len()],
    height: u16,
    shown: u16,
}

impl Laid {
    /// The furthest the body scrolls: the last rows of panels at its foot.
    fn furthest(self) -> u16 {
        self.height.saturating_sub(self.shown)
    }

    /// Each panel's first row and the row past its last.
    fn spans(self) -> impl Iterator<Item = (u16, u16)> {
        self.panels
            .into_iter()
            .flatten()
            .map(|(top, height)| (top, top + height))
    }

    /// The fewest rows a page moves where it can: half a frame.
    fn half(self) -> u16 {
        (self.shown / 2).max(1)
    }

    /// How far a page down from `scroll` scrolls: the first panel the foot
    /// cuts short, or the first beneath it, brought to the top, so that it
    /// reads whole. A page moves a whole frame instead where that would
    /// move it less than half of one, and never more, so a panel taller
    /// than the body is read a frame at a time.
    fn page_down(self, scroll: u16) -> u16 {
        let foot = scroll + self.shown;
        let next = self
            .spans()
            .filter(|(top, end)| *end > foot && *top > scroll)
            .map(|(top, _)| top)
            .min()
            .filter(|top| top - scroll >= self.half())
            .unwrap_or(foot);
        next.min(foot).min(self.furthest())
    }

    /// How far a page up from `scroll` scrolls, as [`Self::page_down`]
    /// pages down: the last panel the top cuts short, or the last above
    /// it, brought to the foot, or a whole frame where that would move less
    /// than half of one, and never more.
    fn page_up(self, scroll: u16) -> u16 {
        let foot = scroll + self.shown;
        let back = scroll.saturating_sub(self.shown);
        self.spans()
            .filter(|(top, end)| *top < scroll && *end < foot)
            .map(|(_, end)| end.saturating_sub(self.shown))
            .max()
            .filter(|previous| scroll - previous >= self.half())
            .map_or(back, |previous| previous.max(back))
    }

    /// How far the body scrolled `scroll` rows down scrolls to show
    /// `panel` whole: as far as it is where it shows it already, and
    /// otherwise only as far as brings it into view, its top first where it
    /// is taller than the body.
    fn showing(self, scroll: u16, panel: InsightsPanel) -> u16 {
        let Some((top, height)) = self.panels[panel.place()] else {
            return scroll;
        };
        if top < scroll || height > self.shown {
            top
        } else if top + height > scroll + self.shown {
            top + height - self.shown
        } else {
            scroll
        }
    }
}

/// The tab's state: the reports it read, kept by range, and the panel the
/// keys act on. What it read does not outlive the view's closing.
#[derive(Clone, Debug, Default)]
pub(in crate::tui) struct InsightsView {
    /// The insights read of each range, kept with the zone it was cut in.
    reports: RangeReads<KeptReport, TimeZone>,
    /// The one-bucket Workspace read made beside it, kept as the
    /// Workspaces ranked as Overview ranks its keys.
    workspaces: RangeReads<Vec<WorkspaceRow>>,
    focused: Option<InsightsPanel>,
    /// The item selected within the focused panel, by its place among the
    /// panel's items.
    selected: Option<usize>,
    /// How many rows down each panel is scrolled, in
    /// [`InsightsPanel::ALL`] order: always none for a panel whose rows all
    /// stand in view.
    scrolled: [usize; InsightsPanel::ALL.len()],
    tools_sort: ToolsSort,
    /// How many rows the panels are scrolled up the body, from their top.
    /// Each frame settles it no further than brings their last rows to its
    /// foot, so a frame or report that grows and shrinks again never jumps
    /// back to an older offset; a new range scrolls back to the top.
    body_scroll: Cell<u16>,
    targets: Cell<Targets>,
}

impl InsightsView {
    /// The report kept for `range` from `origin`, where one is.
    fn report(&self, origin: &Outlook, range: MetricsRange) -> Option<&KeptReport> {
        self.reports.kept(origin, range)
    }

    /// The Workspaces kept for `range` from `origin`, where they are.
    fn workspaces_of(&self, origin: &Outlook, range: MetricsRange) -> Option<&[WorkspaceRow]> {
        self.workspaces.kept(origin, range).map(Vec::as_slice)
    }

    /// Whether `range` lacks a report, or its Workspaces, from `origin`
    /// with neither on its way, so that showing it must read them. A range
    /// whose read was refused lacks one, so showing it again reads it
    /// again.
    pub(in crate::tui) fn lacks(&self, origin: &Outlook, range: MetricsRange) -> bool {
        let lacking =
            self.report(origin, range).is_none() || self.workspaces_of(origin, range).is_none();
        lacking && !self.is_reading(origin, range) && !self.workspaces.is_reading(origin, range)
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
    ) -> Option<InsightsAsk> {
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
        let request_id = self.reports.ask(origin.clone(), range, zone.clone());
        let workspaces_request_id = self.workspaces.ask(origin, range, ());
        Some(InsightsAsk {
            request_id,
            query: InsightsQuery { interval },
            workspaces_request_id,
            workspaces: UsageMetricsQuery {
                buckets: vec![interval],
                breakdown: UsageMetricsBreakdown::Workspace,
                comparison: None,
                top_sessions: 0,
            },
        })
    }

    /// Lets go of every report kept but `range`'s, as a refresh of `range`
    /// does: the others would otherwise show older figures than it beside
    /// nothing saying so, and are read again when next shown. `range`'s own
    /// stays until the refresh replaces it.
    pub(in crate::tui) fn forget_all_but(&mut self, range: MetricsRange) {
        self.reports.forget_all_but(range);
        self.workspaces.forget_all_but(range);
    }

    /// Takes the Server's answer, unless the tab has since closed or asked
    /// again, keeping a report under the range it was asked for. A refusal
    /// gives up the Workspace read beside it too, so that showing the range
    /// again reads both again.
    pub(in crate::tui) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<InsightsReport, UsageMetricsReadRefusal>,
    ) {
        if result.is_err() && self.reports.awaits(request_id) {
            self.workspaces.stop_awaiting();
        }
        self.reports.receive(request_id, result, |report, zone| {
            let heatmap = Heatmap::fold(&report.quarter_hours, &zone);
            KeptReport { report, heatmap }
        });
    }

    /// Whether the tab awaits the Workspace read answering under
    /// `request_id`, which answers as Overview's reads do.
    pub(in crate::tui) fn awaits_workspaces(&self, request_id: Uuid) -> bool {
        self.workspaces.awaits(request_id)
    }

    /// Takes the Server's answer to the Workspace read, unless the tab has
    /// since closed or asked again, keeping its Workspaces ranked under the
    /// range it was asked for.
    pub(in crate::tui) fn receive_workspaces(
        &mut self,
        request_id: Uuid,
        result: Result<UsageMetricsReport, UsageMetricsReadRefusal>,
    ) {
        self.workspaces
            .receive(request_id, result, |report, ()| workspaces::rows(&report));
    }

    /// Says why `range` has no fresh report from `origin`, giving up both
    /// reads awaited. A report already kept for it stays, with the refusal
    /// beside it.
    pub(in crate::tui) fn refuse(
        &mut self,
        origin: Outlook,
        range: MetricsRange,
        refusal: UsageMetricsReadRefusal,
    ) {
        self.reports.refuse(origin, range, refusal);
        self.workspaces.stop_awaiting();
    }

    /// Lets go of the reads awaited, as the view closes.
    pub(in crate::tui) fn stop_awaiting(&mut self) {
        self.reports.stop_awaiting();
        self.workspaces.stop_awaiting();
    }

    /// Whether a read of `range` from `origin` is on its way.
    pub(in crate::tui) fn is_reading(&self, origin: &Outlook, range: MetricsRange) -> bool {
        self.reports.is_reading(origin, range)
    }

    /// What the Workspaces panel shows for `range`, read from `origin`.
    pub(super) fn workspaces_content(
        &self,
        origin: &Outlook,
        range: MetricsRange,
    ) -> WorkspacesContent<'_> {
        match (
            self.workspaces_of(origin, range),
            self.workspaces.refusal(origin, range),
        ) {
            (Some(rows), _) => WorkspacesContent::Rows(rows),
            (None, Some(refusal)) => WorkspacesContent::Refused(refusal),
            (None, None) => WorkspacesContent::Reading,
        }
    }

    /// The key of the `item`-th Workspace the Workspaces panel ranks for
    /// `range` from `origin`, where it ranks that many.
    pub(in crate::tui) fn workspace(
        &self,
        origin: &Outlook,
        range: MetricsRange,
        item: usize,
    ) -> Option<UsageMetricsKey> {
        let row = self.workspaces_of(origin, range)?.get(item)?;
        Some(row.key.clone())
    }

    /// How many items `panel` offers for selection in what is shown for
    /// `range` from `origin`: none before its report lands.
    fn items(&self, origin: &Outlook, range: MetricsRange, panel: InsightsPanel) -> usize {
        let workspaces = self.workspaces_of(origin, range).unwrap_or_default();
        self.report(origin, range)
            .map_or(0, |kept| panel.items(kept, workspaces))
    }

    /// What the tab shows for `range`, read from `origin`.
    pub(super) fn content(&self, origin: &Outlook, range: MetricsRange) -> InsightsContent<'_> {
        match (
            self.report(origin, range),
            self.reports.refusal(origin, range),
        ) {
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
            .and_then(|_| self.reports.refusal(origin, range))
    }

    pub(in crate::tui) fn focused(&self) -> Option<InsightsPanel> {
        self.focused
    }

    /// Focuses `panel`, letting go of what another panel had selected, and
    /// scrolls the body just far enough to show it whole.
    pub(in crate::tui) fn focus(&mut self, panel: InsightsPanel) {
        if self.focused != Some(panel) {
            self.selected = None;
        }
        self.focused = Some(panel);
        if let Some(laid) = self.targets.get().laid {
            let scroll = laid.showing(self.shown_scroll(laid), panel);
            self.body_scroll.set(scroll);
        }
    }

    /// Settles how many rows the panels are scrolled up the body at no
    /// more than `furthest`, as a frame draws them, and answers it.
    pub(super) fn settle_body_scroll(&self, furthest: u16) -> u16 {
        let scroll = self.body_scroll.get().min(furthest);
        self.body_scroll.set(scroll);
        scroll
    }

    /// How far the body laid out as `laid` showed its panels scrolled.
    fn shown_scroll(&self, laid: Laid) -> u16 {
        self.body_scroll.get().min(laid.furthest())
    }

    /// Scrolls the body a page `to`, as the last frame laid the panels out
    /// in it: nowhere where no panels were drawn.
    pub(in crate::tui) fn page(&mut self, to: RowScroll) {
        let Some(laid) = self.targets.get().laid else {
            return;
        };
        let scroll = self.shown_scroll(laid);
        self.body_scroll.set(match to {
            RowScroll::Up => laid.page_up(scroll),
            RowScroll::Down => laid.page_down(scroll),
        });
    }

    /// Scrolls the body `rows` rows `to`, stopping at either end, as the
    /// last frame laid the panels out in it: nowhere where no panels were
    /// drawn.
    pub(in crate::tui) fn scroll_body(&mut self, to: RowScroll, rows: u16) {
        let Some(laid) = self.targets.get().laid else {
            return;
        };
        let scroll = self.shown_scroll(laid);
        self.body_scroll.set(match to {
            RowScroll::Up => scroll.saturating_sub(rows),
            RowScroll::Down => scroll.saturating_add(rows).min(laid.furthest()),
        });
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
        self.body_scroll.set(0);
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
    /// a row `to`, stopping at either end, whether or not it is focused,
    /// and answers whether they moved. A panel whose rows all stand in view
    /// does not scroll.
    pub(in crate::tui) fn scroll(
        &mut self,
        origin: &Outlook,
        range: MetricsRange,
        panel: InsightsPanel,
        to: RowScroll,
    ) -> bool {
        let Some(room) = panel.room() else {
            return false;
        };
        let rows = self.items(origin, range, panel).div_ceil(panel.across());
        let scrolled = self.scrolled(panel);
        let window = RowWindow::new(rows, room, scrolled);
        let first = match to {
            RowScroll::Up => window.first.saturating_sub(1),
            RowScroll::Down => RowWindow::new(rows, room, window.first + 1).first,
        };
        self.scrolled[panel.place()] = first;
        first != window.first
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
        let Some(last) = self.items(origin, range, panel).checked_sub(1) else {
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
        let items = self.items(origin, range, panel);
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

    /// Moves what the panels recorded, drawn from the top of a body
    /// `height` rows tall, to where it is shown through `body`, scrolled
    /// `scroll` rows down: each part cut to the rows shown, and forgotten
    /// where none of it is, so a pointer acts only on what it is over.
    pub(super) fn show_targets(&self, body: Rect, scroll: u16, height: u16) {
        self.update_targets(|targets| {
            let window = Rect::new(body.x, scroll, body.width, body.height);
            let shown = |area: Rect| {
                let cut = area.intersection(window);
                (!cut.is_empty()).then(|| Rect {
                    y: cut.y - scroll + body.y,
                    ..cut
                })
            };
            targets.laid = Some(Laid {
                panels: targets
                    .panels
                    .map(|area| area.map(|area| (area.y, area.height))),
                height,
                shown: body.height,
            });
            targets.panels = targets.panels.map(|area| area.and_then(shown));
            for (items, panel) in targets.items.iter_mut().zip(InsightsPanel::ALL) {
                *items = items.and_then(|(area, width, first)| {
                    let cut = shown(area)?;
                    // Each row scrolled out of view above takes its items.
                    let above = usize::from(cut.y + scroll - body.y - area.y);
                    Some((cut, width, first + above * panel.across()))
                });
            }
            targets.body = Some(Rect {
                height: body.height.min(height - scroll),
                ..body
            });
        });
    }

    fn update_targets(&self, update: impl FnOnce(&mut Targets)) {
        let mut targets = self.targets.get();
        update(&mut targets);
        self.targets.set(targets);
    }

    /// What the last frame drew at `position` that a pointer can act on.
    pub(in crate::tui) fn target_at(&self, position: Position) -> Option<InsightsTarget> {
        let targets = self.targets.get();
        let Some(panel) = InsightsPanel::ALL.into_iter().find(|panel| {
            targets.panels[panel.place()].is_some_and(|area| area.contains(position))
        }) else {
            return targets
                .body
                .filter(|body| body.contains(position))
                .map(|_| InsightsTarget::Body);
        };
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
    use super::{InsightsContent, InsightsPanel, InsightsView, Laid, MetricsRange, Outlook};
    use crate::protocol::{InsightsReport, SessionTimestamp};

    /// Panels laid out at each of `spans` — a first row and a height — in a
    /// body showing `shown` rows at once.
    fn laid(shown: u16, spans: &[(u16, u16)]) -> Laid {
        let mut panels = [None; InsightsPanel::ALL.len()];
        for (panel, span) in panels.iter_mut().zip(spans) {
            *panel = Some(*span);
        }
        let height = spans.iter().map(|(top, height)| top + height).max();
        Laid {
            panels,
            height: height.unwrap_or(0),
            shown,
        }
    }

    #[test]
    fn a_page_brings_the_panel_cut_short_to_the_far_edge_and_a_page_back_returns() {
        // When you work, Turns, Cache, Compactions, Approvals, Workspaces,
        // and Tools, stacked in an 18-row body.
        let laid = laid(
            18,
            &[
                (0, 11),
                (12, 5),
                (18, 4),
                (23, 4),
                (28, 6),
                (35, 7),
                (43, 8),
            ],
        );

        assert_eq!(laid.page_down(0), 18, "Cache, cut short at the foot");
        assert_eq!(laid.page_up(18), 0, "and back");
        assert_eq!(laid.page_down(18), 33, "no further than the last rows");
        assert_eq!(laid.page_up(33), 16, "Approvals, cut short at the top");
    }

    #[test]
    fn a_panel_taller_than_the_body_pages_a_frame_at_a_time_either_way() {
        let laid = laid(20, &[(5, 35), (41, 19)]);

        // Bringing the tall panel to the top would move only five rows.
        assert_eq!(laid.page_down(0), 20);
        assert_eq!(laid.page_down(20), 40);
        assert_eq!(laid.page_up(40), 20);
        assert_eq!(laid.page_up(20), 0);
    }

    #[test]
    fn a_page_past_either_end_stays_there() {
        let ends = laid(18, &[(0, 11), (12, 5), (18, 4), (23, 4)]);

        assert_eq!(ends.page_up(0), 0);
        assert_eq!(ends.page_down(ends.furthest()), ends.furthest());
        assert_eq!(laid(18, &[(0, 11), (12, 5)]).page_down(0), 0, "all in view");
    }

    /// The layout places each panel once, so each has a place of its own.
    #[test]
    fn the_layout_places_every_panel_once() {
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
        let request_id = view
            .ask(
                Outlook::Local,
                range,
                SessionTimestamp(1_800_000_000_000),
                &jiff::tz::TimeZone::UTC,
            )
            .expect("a placeable moment")
            .request_id;
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
