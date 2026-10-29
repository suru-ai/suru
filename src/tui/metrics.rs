//! The Usage Metrics view: what the Sessions living on the Server the
//! Outlook is turned toward consumed over a span of time, read on request
//! and drawn over the whole frame. Nothing in it updates live; a refresh
//! reads again.
//!
//! It has two tabs sharing one range: Overview, which charts what was
//! consumed, and Insights, whose panels say how the work went. Each reads
//! its own report when it is shown and lacks one.

pub(crate) mod calendar;
pub(crate) mod chart;
mod headline;
pub(crate) mod insights;
mod mix;
mod view;

use std::{cell::Cell, collections::HashSet};

use jiff::tz::TimeZone;
use ratatui::layout::{Position, Rect};
use uuid::Uuid;

use crate::protocol::{
    Outlook, SessionId, SessionReference, SessionTimestamp, UsageMetricsBreakdown,
    UsageMetricsInterval, UsageMetricsKey, UsageMetricsQuery, UsageMetricsReport,
};

use calendar::{MetricsRange, RangeBuckets};
use chart::{MetricsMeasure, SeriesId};
use insights::{InsightsTarget, InsightsView};

pub(super) use view::render;

/// How many of the Sessions that cost the most the view asks for: five,
/// which the list draws fewer of where it has less room.
const TOP_SESSIONS: usize = 5;

/// The most series a legend lists: every key a chart keeps, and Other.
pub(super) const LEGEND_SERIES: usize = chart::KEYED_SERIES + 1;

/// Every breakdown, in the order the view offers them.
pub(super) const BREAKDOWNS: [UsageMetricsBreakdown; 3] = [
    UsageMetricsBreakdown::Model,
    UsageMetricsBreakdown::Provider,
    UsageMetricsBreakdown::Workspace,
];

/// A breakdown as the view offers it: in turn among [`BREAKDOWNS`], and in
/// the words its controls and notes use.
pub(super) trait OfferedBreakdown: Sized {
    /// The breakdown after this one, the last turning back to the first.
    fn next(self) -> Self;
    /// Where the breakdown stands in [`BREAKDOWNS`].
    fn place(self) -> usize;
    /// What one of its keys is called.
    fn label(self) -> &'static str;
    /// What several of its keys are called.
    fn plural(self) -> &'static str;
}

impl OfferedBreakdown for UsageMetricsBreakdown {
    fn next(self) -> Self {
        BREAKDOWNS[(self.place() + 1) % BREAKDOWNS.len()]
    }

    fn place(self) -> usize {
        BREAKDOWNS
            .iter()
            .position(|breakdown| *breakdown == self)
            .expect("every breakdown is offered")
    }

    fn label(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::Provider => "Provider",
            Self::Workspace => "Workspace",
        }
    }

    fn plural(self) -> &'static str {
        match self {
            Self::Model => "Models",
            Self::Provider => "Providers",
            Self::Workspace => "Workspaces",
        }
    }
}

/// A tab of the view, beneath its title bar and above the range bar.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum MetricsTab {
    /// The chart of what was consumed, where the view opens.
    #[default]
    Overview,
    /// Panels saying how the work went.
    Insights,
}

impl MetricsTab {
    /// Every tab, in the order the tab bar lists them.
    pub const ALL: [Self; 2] = [Self::Overview, Self::Insights];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Insights => "Insights",
        }
    }

    pub(crate) fn place(self) -> usize {
        Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("every tab is listed")
    }

    /// The tab after this one, or this one where it is the last.
    pub(crate) fn next(self) -> Self {
        Self::ALL[(self.place() + 1).min(Self::ALL.len() - 1)]
    }

    /// The tab before this one, or this one where it is the first.
    pub(crate) fn previous(self) -> Self {
        Self::ALL[self.place().saturating_sub(1)]
    }
}

/// Why the view has no fresh figures to show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricsReadRefusal {
    /// The Server answered, and refused the read, in its own words.
    Refused(String),
    /// The read never reached a Server that could answer it, for the reason
    /// given: an Origin that stopped answering, or a request that failed.
    Failed(String),
}

impl MetricsReadRefusal {
    pub(super) fn reason(&self) -> &str {
        match self {
            Self::Refused(reason) | Self::Failed(reason) => reason,
        }
    }
}

/// The view's state. Its choices outlive its closing, for the rest of the
/// TUI process; what it read does not, since the next opening reads again.
#[derive(Clone, Debug, Default)]
pub(super) struct MetricsView {
    open: bool,
    tab: MetricsTab,
    /// The range both tabs show.
    range: MetricsRange,
    measure: MetricsMeasure,
    breakdown: UsageMetricsBreakdown,
    /// The read the view awaits, the Outlook it asked, and what it asked for.
    asked: Option<(Uuid, Outlook, Asked)>,
    /// The last report read, with the Outlook it was read from and the
    /// buckets it answers: shown only while the Outlook names that Server.
    shown: Option<(Outlook, RangeBuckets, UsageMetricsReport)>,
    /// Why the last read gave no report.
    refusal: Option<MetricsReadRefusal>,
    /// The bucket whose figures the legend shows in place of the range's,
    /// by its place among the shown report's buckets.
    selected: Option<usize>,
    /// The series the chart leaves out of its stacks, by which series they
    /// are rather than where they stand, so they stay hidden through a
    /// change of measure or a refresh. Only a new range or breakdown, whose
    /// series are others, lets them go.
    hidden: HashSet<SeriesId>,
    /// A key the chart keeps as a series of its own even where it would
    /// fold into Other, so that it can be shown alone: the Workspace an
    /// Insights row opened. Only a new range or breakdown lets it go.
    kept_apart: Option<UsageMetricsKey>,
    /// The series to show alone once a report holding it lands, where it
    /// was asked for before one had.
    alone: Option<SeriesId>,
    /// The row of one of the view's lists the keyboard acts on.
    focused: Option<ListRow>,
    /// When the earliest work was placed — a timed Turn, or an Errand —
    /// as the last report since opening said, from which All time begins.
    earliest: EarliestWork,
    /// Where the last frame drew what a pointer can act on.
    targets: Cell<Targets>,
    /// The Insights tab's reports and focus.
    insights: InsightsView,
}

/// A row of one of the view's lists, by what it shows rather than where it
/// stands, so a focus outlasts a change of measure or a refresh that moves
/// it. ↑/↓ walk the rows of every list in the order they stand on screen,
/// from the legend down to the Sessions that cost the most.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ListRow {
    /// A legend row, by its series.
    Series(SeriesId),
    /// A row of the Sessions that cost the most, by its Session.
    Session(SessionId),
}

/// The focused row as the keys act on it: by its place among its list's
/// rows, which is how the commands they invoke name it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusedRow {
    /// The legend row at that place, counted from the first.
    Series(usize),
    /// The row of the Sessions that cost the most at that place, counted
    /// from the first.
    Session(usize),
}

/// Where a list focus moves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusMove {
    /// The row before, or the last where none is focused.
    Previous,
    /// The row after, or the first where none is focused.
    Next,
}

/// What a read the view awaits asked for.
#[derive(Clone, Debug)]
enum Asked {
    /// No buckets at all, only when the earliest work was placed, which
    /// All time must learn before it can cut its own.
    EarliestWork,
    /// The range's buckets, as the calendar cut them.
    Range(RangeBuckets),
}

/// What the view knows of the earliest work the Server places in time: a
/// timed Turn, or an Errand.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EarliestWork {
    /// No report since the view opened has said.
    #[default]
    Unknown,
    /// A report said no work is placed in time.
    Untimed,
    At(SessionTimestamp),
}

impl From<Option<SessionTimestamp>> for EarliestWork {
    fn from(earliest: Option<SessionTimestamp>) -> Self {
        earliest.map_or(Self::Untimed, Self::At)
    }
}

/// What the view needs once an answer lands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Receipt {
    /// Nothing more: the answer is shown, refused, or was not awaited.
    Settled,
    /// To read again, since All time has learned where it begins.
    ReadAgain,
}

/// Where the last frame drew each part of the view a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    close: Option<Rect>,
    /// Each tab's place in the tab bar, in [`MetricsTab::ALL`] order.
    tabs: [Option<Rect>; MetricsTab::ALL.len()],
    /// Each range's place in the range bar, in [`MetricsRange::ALL`] order.
    ranges: [Option<Rect>; MetricsRange::ALL.len()],
    /// Each measure's place beside it, in [`MetricsMeasure::ALL`] order.
    measures: [Option<Rect>; MetricsMeasure::ALL.len()],
    /// Each breakdown's place after them, in [`BREAKDOWNS`] order.
    breakdowns: [Option<Rect>; BREAKDOWNS.len()],
    /// The chart, or where it stands while there is none to draw.
    chart: Option<Rect>,
    /// The bars the chart drew, each a column of it a pointer selects by.
    bars: Option<BarStrip>,
    /// Where it drew each of the legend's series, in their order — a row
    /// apiece, or a place along the lines they flow on — none past the last
    /// it had room for, and nothing at all before any frame has drawn a
    /// report.
    legend: Option<[Option<Rect>; LEGEND_SERIES]>,
    /// The rows of the Sessions that cost the most it drew, one Session to a
    /// row, on the same terms as the legend's.
    sessions: Option<Rect>,
}

/// Where the last frame drew the chart's bars: `count` slots `slot` columns
/// wide side by side across `area`, from its left edge, each a bar and the
/// gap before it, over every row of the chart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BarStrip {
    pub(super) area: Rect,
    pub(super) slot: u16,
    pub(super) count: usize,
}

impl BarStrip {
    /// The bucket whose slot holds column `x`, the first or the last for a
    /// column before or past them all.
    fn nearest(self, x: u16) -> usize {
        let offset = usize::from(x.saturating_sub(self.area.x) / self.slot.max(1));
        offset.min(self.count.saturating_sub(1))
    }
}

/// A part of the view a pointer can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetricsTarget {
    Close,
    Tab(MetricsTab),
    Range(MetricsRange),
    Measure(MetricsMeasure),
    Breakdown(UsageMetricsBreakdown),
    /// A bucket's bar, by its place among them, or the space above or
    /// before it in its slot.
    Bar(usize),
    /// The chart away from any bar, `toward` the bar a drag across it
    /// reaches for: the nearest along its row, where any is drawn.
    Chart {
        toward: Option<usize>,
    },
    /// A legend row, by its series' place among them.
    Series(usize),
    /// A row of the Sessions that cost the most, by its place among them.
    Session(usize),
    /// Something the Insights tab drew.
    Insights(InsightsTarget),
}

/// Where a bucket selection moves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BucketMove {
    /// The bucket before, or the last where none is selected.
    Previous,
    /// The bucket after, or the first where none is selected.
    Next,
    First,
    Last,
    /// The bucket at that place, or the last for a place past it.
    To(usize),
}

/// What the view shows beneath its title bar.
pub(super) enum MetricsContent<'a> {
    Reading,
    Refused(&'a MetricsReadRefusal),
    Report {
        buckets: &'a RangeBuckets,
        report: &'a UsageMetricsReport,
    },
}

impl MetricsView {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the view on Overview with nothing yet read, keeping the
    /// choices it had.
    pub(super) fn open(&mut self) {
        self.open = true;
        self.tab = MetricsTab::Overview;
        self.insights = InsightsView::default();
        self.asked = None;
        self.shown = None;
        self.refusal = None;
        self.selected = None;
        self.hidden.clear();
        self.kept_apart = None;
        self.alone = None;
        self.focused = None;
        self.earliest = EarliestWork::Unknown;
    }

    /// Lets go of every figure, and every read awaited, that is not of the
    /// Server `outlook` names, on both tabs, as the Outlook turns toward it:
    /// the tabs describe one Server, and figures of the one turned from must
    /// not stand under the other's name. When All time begins is that
    /// Server's to say again.
    pub(super) fn turn_toward(&mut self, outlook: &Outlook) {
        let elsewhere = self
            .shown
            .as_ref()
            .map(|(origin, _, _)| origin)
            .into_iter()
            .chain(self.asked.as_ref().map(|(_, origin, _)| origin))
            .any(|origin| origin != outlook);
        if elsewhere {
            self.forget_figures();
            self.earliest = EarliestWork::Unknown;
        }
        self.insights.keep_only(outlook);
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.asked = None;
        self.insights.stop_awaiting();
        self.forget_targets();
    }

    pub(super) fn tab(&self) -> MetricsTab {
        self.tab
    }

    /// Shows `tab`, answering whether that changed it.
    pub(super) fn choose_tab(&mut self, tab: MetricsTab) -> bool {
        let changed = self.tab != tab;
        self.tab = tab;
        changed
    }

    /// Whether the Overview has no report from `outlook` to show nor one
    /// on its way, so showing it must read: a read that was refused is read
    /// again, as Insights reads a refused range again.
    pub(super) fn overview_lacks_report(&self, outlook: &Outlook) -> bool {
        let shown = self
            .shown
            .as_ref()
            .is_some_and(|(origin, _, _)| origin == outlook);
        let asked = self
            .asked
            .as_ref()
            .is_some_and(|(_, origin, _)| origin == outlook);
        !shown && !asked
    }

    /// Lets go of the Overview's report, as a refresh of Insights does: it
    /// would otherwise show older figures than Insights beside nothing
    /// saying so, and is read again when next shown.
    pub(super) fn forget_overview_report(&mut self) {
        self.shown = None;
        self.selected = None;
    }

    pub(super) fn insights(&self) -> &InsightsView {
        &self.insights
    }

    pub(super) fn insights_mut(&mut self) -> &mut InsightsView {
        &mut self.insights
    }

    /// Says why the tab showing has no fresh figures from `origin`.
    pub(super) fn refuse_showing(&mut self, origin: Outlook, refusal: MetricsReadRefusal) {
        match self.tab {
            MetricsTab::Overview => self.refuse(refusal),
            MetricsTab::Insights => self.insights.refuse(origin, self.range, refusal),
        }
    }

    /// Chooses the range the view shows, answering whether that changed it,
    /// so that it must be read anew. Figures for another range are not shown
    /// beside the new range's name while they are.
    pub(super) fn choose_range(&mut self, range: MetricsRange) -> bool {
        if self.range == range {
            return false;
        }
        self.range = range;
        self.forget_figures();
        // An item a panel selected stands for figures of the range before,
        // and must not outlast them unseen for Esc to clear; how far a
        // panel was scrolled stands for them too.
        self.insights.leave_range();
        true
    }

    /// Chooses how the view splits its bars, answering whether that changed
    /// it, so that it must be read anew: a report holds the keys of the
    /// breakdown it was asked for alone, and is not shown under another's
    /// name while the new one is read.
    pub(super) fn choose_breakdown(&mut self, breakdown: UsageMetricsBreakdown) -> bool {
        if self.breakdown == breakdown {
            return false;
        }
        self.breakdown = breakdown;
        self.forget_figures();
        true
    }

    /// Lets go of every figure the view holds or awaits, once what it shows
    /// is chosen anew: an answer to a read asked before the choice answers
    /// for another, and must not land under the new choice's name even
    /// where the read that choice brings is never asked.
    fn forget_figures(&mut self) {
        self.asked = None;
        self.shown = None;
        self.refusal = None;
        self.selected = None;
        self.hidden.clear();
        self.kept_apart = None;
        self.alone = None;
        self.focused = None;
    }

    /// How many buckets a selection can move among: those of the report
    /// shown that its chart draws, so a selection always has its bar and
    /// mark. A frame too small for the view draws none, and one too narrow
    /// for every bar would draw the first few; before any frame has drawn
    /// the report's bars, all of them count.
    fn selectable(&self) -> usize {
        let buckets = match &self.shown {
            Some((_, buckets, report)) if !chart::holds_nothing(report) => buckets.buckets.len(),
            _ => return 0,
        };
        self.targets
            .get()
            .bars
            .map_or(buckets, |bars| bars.count.min(buckets))
    }

    /// The bucket chosen, where one is, whether or not the last frame drew
    /// its bar: what Esc clears before it closes the view.
    pub(super) fn selected_bucket(&self) -> Option<usize> {
        self.selected
    }

    /// The bucket the chart marks and the legend reads: the one chosen, or
    /// the last bar drawn where it is past them, and none where no bar is
    /// drawn. Before any frame has drawn the report's bars, the one chosen.
    pub(super) fn drawn_selection(&self) -> Option<usize> {
        let last = self.selectable().checked_sub(1)?;
        self.selected.map(|selected| selected.min(last))
    }

    /// Moves the bucket selection, which stops at the first and last
    /// bucket drawn, from the last where the one selected is past it. With
    /// no bars drawn there is nothing to select.
    pub(super) fn move_selection(&mut self, to: BucketMove) {
        let Some(last) = self.selectable().checked_sub(1) else {
            return;
        };
        self.selected = Some(match (to, self.drawn_selection()) {
            (BucketMove::Previous, Some(selected)) => selected.saturating_sub(1),
            (BucketMove::Next, Some(selected)) => (selected + 1).min(last),
            (BucketMove::Previous | BucketMove::Last, _) => last,
            (BucketMove::Next | BucketMove::First, _) => 0,
            (BucketMove::To(bucket), _) => bucket.min(last),
        });
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// The series of the report shown, in the legend's order, none where
    /// it holds nothing to draw.
    fn series_ids(&self) -> Vec<SeriesId> {
        match &self.shown {
            Some((_, _, report)) if !chart::holds_nothing(report) => {
                chart::series_ids(report, self.kept_apart.as_ref())
            }
            _ => Vec::new(),
        }
    }

    /// The Sessions the report shown names as costing the most, in its
    /// order, none where it holds nothing to draw.
    fn top_session_ids(&self) -> Vec<SessionId> {
        match &self.shown {
            Some((_, _, report)) if !chart::holds_nothing(report) => report
                .top_sessions
                .iter()
                .map(|session| session.session_id)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Every row of the view's lists a focus can rest on, in the order they
    /// stand on screen: those the last frame drew, so a focus always has its
    /// row, or every one before any frame has drawn them.
    fn list_rows(&self) -> Vec<ListRow> {
        let targets = self.targets.get();
        let legend = targets
            .legend
            .map_or(usize::MAX, |items| items.iter().flatten().count());
        let sessions = targets
            .sessions
            .map_or(usize::MAX, |rows| usize::from(rows.height));
        let series = self
            .series_ids()
            .into_iter()
            .take(legend)
            .map(ListRow::Series);
        let sessions = self
            .top_session_ids()
            .into_iter()
            .take(sessions)
            .map(ListRow::Session);
        series.chain(sessions).collect()
    }

    /// Moves the focus among the rows of the view's lists, which stops at
    /// the first and last row. With no rows there is nothing to focus.
    pub(super) fn move_focus(&mut self, to: FocusMove) {
        let rows = self.list_rows();
        let Some(last) = rows.len().checked_sub(1) else {
            return;
        };
        let at = self
            .focused
            .as_ref()
            .and_then(|focused| rows.iter().position(|row| row == focused));
        let place = match (to, at) {
            (FocusMove::Previous, Some(at)) => at.saturating_sub(1),
            (FocusMove::Next, Some(at)) => (at + 1).min(last),
            (FocusMove::Previous, None) => last,
            (FocusMove::Next, None) => 0,
        };
        self.focused = rows.into_iter().nth(place);
    }

    pub(super) fn clear_focus(&mut self) {
        self.focused = None;
    }

    /// The focused row, by its place in its list, where one is focused.
    pub(super) fn focused_row(&self) -> Option<FocusedRow> {
        match self.focused.as_ref()? {
            ListRow::Series(id) => self
                .series_ids()
                .iter()
                .position(|series| series == id)
                .map(FocusedRow::Series),
            ListRow::Session(id) => self
                .top_session_ids()
                .iter()
                .position(|session| session == id)
                .map(FocusedRow::Session),
        }
    }

    /// The `place`-th of the Sessions the view lists as costing the most,
    /// on the Server the report naming it was read from — the one the
    /// Outlook names, since the view lets go of any other's report as the
    /// Outlook turns — where it lists that many.
    pub(super) fn top_session(&self, place: usize) -> Option<SessionReference> {
        let (origin, _, _) = self.shown.as_ref()?;
        let session_id = *self.top_session_ids().get(place)?;
        Some(SessionReference::new(origin.clone(), session_id))
    }

    /// Hides the `place`-th series in the legend from the chart, or shows it
    /// again where it was hidden, focusing its row, as a click on it does. A
    /// place past the last names none.
    pub(super) fn toggle_series(&mut self, place: usize) {
        let Some(id) = self.series_ids().into_iter().nth(place) else {
            return;
        };
        self.focused = Some(ListRow::Series(id.clone()));
        if !self.hidden.remove(&id) {
            self.hidden.insert(id);
        }
    }

    /// Shows the `place`-th series in the legend alone, hiding every other,
    /// or, where it is already the only one shown, shows them all again,
    /// focusing its row.
    pub(super) fn solo_series(&mut self, place: usize) {
        let ids = self.series_ids();
        let Some(id) = ids.get(place) else {
            return;
        };
        self.focused = Some(ListRow::Series(id.clone()));
        let alone = ids
            .iter()
            .all(|other| (other == id) != self.is_hidden(other));
        self.hidden = if alone {
            HashSet::new()
        } else {
            ids.iter().filter(|other| *other != id).cloned().collect()
        };
    }

    /// Shows `key`'s series alone, hiding every other, and focuses its row,
    /// whatever was shown before: the way into a key from outside the
    /// legend, by which key rather than where its row stands. The key is
    /// kept apart from Other, as a series of its own in place of the
    /// smallest key kept, so that a key that would fold away is shown alone
    /// rather than among the keys folded with it. Before a report has landed
    /// for the breakdown, the key is shown alone once one does; a report
    /// without the key shows every series, and no later one hides any.
    pub(super) fn show_alone(&mut self, key: UsageMetricsKey) {
        self.kept_apart = Some(key.clone());
        self.alone = Some(SeriesId::Key(key));
        if self.shown.is_some() {
            self.show_awaited_alone();
        }
    }

    /// Shows the series awaiting it alone, where the report shown has it,
    /// and stops awaiting it either way.
    fn show_awaited_alone(&mut self) {
        let Some(id) = self.alone.take() else {
            return;
        };
        let ids = self.series_ids();
        if ids.contains(&id) {
            self.hidden = ids.into_iter().filter(|other| *other != id).collect();
            self.focused = Some(ListRow::Series(id));
        }
    }

    /// The key the chart keeps as a series of its own, where one is.
    pub(super) fn kept_apart(&self) -> Option<&UsageMetricsKey> {
        self.kept_apart.as_ref()
    }

    /// Whether the chart leaves `series` out of its stacks.
    pub(super) fn is_hidden(&self, series: &SeriesId) -> bool {
        self.hidden.contains(series)
    }

    pub(super) fn breakdown(&self) -> UsageMetricsBreakdown {
        self.breakdown
    }

    /// The query `origin` is asked for the view's range at `now` in `zone`,
    /// compared with the
    /// period before it as the calendar cuts that, which the view now awaits
    /// the answer to under the request it answers with. A report
    /// already shown stays until the answer replaces it. A moment the
    /// calendar cannot place is refused in the view instead.
    ///
    /// All time begins with the earliest timed work's month, so before any
    /// report has said when that was, it asks for no buckets at all: a read
    /// that answers with nothing but the earliest work, from which
    /// [`Self::receive`] has the view read again.
    pub(super) fn ask(
        &mut self,
        origin: Outlook,
        now: SessionTimestamp,
        zone: &TimeZone,
    ) -> Option<(Uuid, UsageMetricsQuery)> {
        let earliest = match self.earliest {
            EarliestWork::Unknown if self.range == MetricsRange::AllTime => {
                return Some(self.await_answer(origin, Asked::EarliestWork, Vec::new(), None));
            }
            EarliestWork::At(earliest) => Some(earliest),
            EarliestWork::Unknown | EarliestWork::Untimed => None,
        };
        let Some(buckets) = calendar::resolve(self.range, now, zone, earliest) else {
            self.refuse(MetricsReadRefusal::Failed(
                "The clock reads a moment the calendar cannot place".to_owned(),
            ));
            return None;
        };
        let intervals = buckets
            .buckets
            .iter()
            .map(|bucket| bucket.interval)
            .collect();
        let comparison = buckets.comparison;
        Some(self.await_answer(origin, Asked::Range(buckets), intervals, comparison))
    }

    fn await_answer(
        &mut self,
        origin: Outlook,
        asked: Asked,
        buckets: Vec<UsageMetricsInterval>,
        comparison: Option<UsageMetricsInterval>,
    ) -> (Uuid, UsageMetricsQuery) {
        let request_id = Uuid::new_v4();
        let query = UsageMetricsQuery {
            buckets,
            breakdown: self.breakdown,
            comparison,
            top_sessions: TOP_SESSIONS,
        };
        self.asked = Some((request_id, origin, asked));
        self.refusal = None;
        (request_id, query)
    }

    /// Takes the Server's answer, unless the view has since closed or asked
    /// again, and says whether the view must read again: after All time has
    /// learned when the earliest work was, or of work earlier than the
    /// buckets it asked for begin.
    pub(super) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<UsageMetricsReport, MetricsReadRefusal>,
    ) -> Receipt {
        if !self
            .asked
            .as_ref()
            .is_some_and(|(asked, _, _)| *asked == request_id)
        {
            return Receipt::Settled;
        }
        let (_, origin, asked) = self.asked.take().expect("the awaited read");
        let report = match result {
            Ok(report) => report,
            Err(refusal) => {
                self.refuse(refusal);
                return Receipt::Settled;
            }
        };
        let earliest = EarliestWork::from(report.earliest_at);
        // Asked again only for earliest work the cut did not begin from,
        // so a read whose buckets cannot reach back that far is asked once.
        let learned = self.earliest != earliest;
        self.earliest = earliest;
        let buckets = match asked {
            Asked::EarliestWork => return Receipt::ReadAgain,
            Asked::Range(buckets) => buckets,
        };
        let uncovered = report.earliest_at.is_some_and(|earliest| {
            buckets
                .buckets
                .first()
                .is_none_or(|first| earliest < first.interval.start)
        });
        if self.range == MetricsRange::AllTime && learned && uncovered {
            return Receipt::ReadAgain;
        }
        // A refresh keeps the bucket chosen by when it begins, which a later
        // clock may have moved along the range or out of it altogether.
        let chosen = self
            .selected
            .zip(self.shown.as_ref())
            .and_then(|(selected, (_, shown, _))| shown.buckets.get(selected))
            .map(|bucket| bucket.interval.start);
        self.shown = Some((origin, buckets, report));
        self.refusal = None;
        let selectable = self.selectable();
        self.selected = chosen.and_then(|start| {
            self.shown
                .as_ref()?
                .1
                .buckets
                .iter()
                .position(|bucket| bucket.interval.start == start)
                .filter(|place| *place < selectable)
        });
        self.show_awaited_alone();
        Receipt::Settled
    }

    /// Says why the view has no fresh figures, giving up any read it
    /// awaits. A report already shown stays, with the refusal beside it.
    pub(super) fn refuse(&mut self, refusal: MetricsReadRefusal) {
        self.asked = None;
        self.refusal = Some(refusal);
    }

    /// Why the figures shown could not be read again, where they could not.
    pub(super) fn stale_because(&self) -> Option<&MetricsReadRefusal> {
        self.shown.as_ref().and(self.refusal.as_ref())
    }

    pub(super) fn range(&self) -> MetricsRange {
        self.range
    }

    pub(super) fn measure(&self) -> MetricsMeasure {
        self.measure
    }

    /// Chooses what the chart plots, from the figures the view already
    /// holds: a report carries every measure.
    pub(super) fn choose_measure(&mut self, measure: MetricsMeasure) {
        self.measure = measure;
    }

    /// Whether a read is on its way.
    pub(super) fn is_reading(&self) -> bool {
        self.asked.is_some()
    }

    /// What the Overview shows of the Server `outlook` names: never a
    /// report read from another.
    pub(super) fn content(&self, outlook: &Outlook) -> MetricsContent<'_> {
        let shown = self
            .shown
            .as_ref()
            .filter(|(origin, _, _)| origin == outlook);
        match (shown, &self.refusal) {
            (Some((_, buckets, report)), _) => MetricsContent::Report { buckets, report },
            (None, Some(refusal)) => MetricsContent::Refused(refusal),
            (None, None) => MetricsContent::Reading,
        }
    }

    /// Forgets where the last frame drew anything, before the next draws.
    pub(super) fn forget_targets(&self) {
        self.targets.set(Targets::default());
        self.insights.forget_targets();
    }

    pub(super) fn record_close_target(&self, area: Rect) {
        self.update_targets(|targets| targets.close = Some(area));
    }

    pub(super) fn record_tab_target(&self, tab: MetricsTab, area: Rect) {
        self.update_targets(|targets| targets.tabs[tab.place()] = Some(area));
    }

    pub(super) fn record_range_target(&self, range: MetricsRange, area: Rect) {
        self.update_targets(|targets| targets.ranges[range.place()] = Some(area));
    }

    pub(super) fn record_measure_target(&self, measure: MetricsMeasure, area: Rect) {
        self.update_targets(|targets| targets.measures[measure.place()] = Some(area));
    }

    pub(super) fn record_breakdown_target(&self, breakdown: UsageMetricsBreakdown, area: Rect) {
        self.update_targets(|targets| targets.breakdowns[breakdown.place()] = Some(area));
    }

    pub(super) fn record_chart_target(&self, area: Rect) {
        self.update_targets(|targets| targets.chart = Some(area));
    }

    pub(super) fn record_bar_targets(&self, bars: BarStrip) {
        self.update_targets(|targets| targets.bars = Some(bars));
    }

    pub(super) fn record_legend_targets(&self, items: [Option<Rect>; LEGEND_SERIES]) {
        self.update_targets(|targets| targets.legend = Some(items));
    }

    pub(super) fn record_session_targets(&self, rows: Rect) {
        self.update_targets(|targets| targets.sessions = Some(rows));
    }

    fn update_targets(&self, update: impl FnOnce(&mut Targets)) {
        let mut targets = self.targets.get();
        update(&mut targets);
        self.targets.set(targets);
    }

    /// What the last frame drew at `position` that a pointer can act on.
    pub(super) fn target_at(&self, position: Position) -> Option<MetricsTarget> {
        let targets = self.targets.get();
        let on = |area: Option<Rect>| area.is_some_and(|area| area.contains(position));
        if on(targets.close) {
            return Some(MetricsTarget::Close);
        }
        if let Some(tab) = MetricsTab::ALL
            .into_iter()
            .find(|tab| on(targets.tabs[tab.place()]))
        {
            return Some(MetricsTarget::Tab(tab));
        }
        if let Some(range) = MetricsRange::ALL
            .into_iter()
            .find(|range| on(targets.ranges[range.place()]))
        {
            return Some(MetricsTarget::Range(range));
        }
        if self.tab == MetricsTab::Insights {
            return self
                .insights
                .target_at(position)
                .map(MetricsTarget::Insights);
        }
        if let Some(measure) = MetricsMeasure::ALL
            .into_iter()
            .find(|measure| on(targets.measures[measure.place()]))
        {
            return Some(MetricsTarget::Measure(measure));
        }
        if let Some(breakdown) = BREAKDOWNS
            .into_iter()
            .find(|breakdown| on(targets.breakdowns[breakdown.place()]))
        {
            return Some(MetricsTarget::Breakdown(breakdown));
        }
        // Beside the bars, out to the frame's edges, the chart reaches for
        // the nearest of them, so a drag past either end stays at that end.
        if let Some(bars) = targets
            .bars
            .filter(|bars| (bars.area.top()..bars.area.bottom()).contains(&position.y))
        {
            let nearest = bars.nearest(position.x);
            return Some(if bars.area.contains(position) {
                MetricsTarget::Bar(nearest)
            } else {
                MetricsTarget::Chart {
                    toward: Some(nearest),
                }
            });
        }
        if let Some(place) = targets.legend.and_then(|items| {
            items
                .iter()
                .position(|item| item.is_some_and(|item| item.contains(position)))
        }) {
            return Some(MetricsTarget::Series(place));
        }
        if let Some(rows) = targets.sessions.filter(|rows| rows.contains(position)) {
            return Some(MetricsTarget::Session(usize::from(position.y - rows.y)));
        }
        on(targets.chart).then_some(MetricsTarget::Chart { toward: None })
    }
}
