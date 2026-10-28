//! The Usage Metrics view: what the Sessions living on the Server the
//! Outlook is turned toward consumed over a span of time, read on request
//! and drawn over the whole frame. Nothing in it updates live; a refresh
//! reads again.

pub(crate) mod calendar;
pub(crate) mod chart;
mod view;

use std::cell::Cell;

use jiff::tz::TimeZone;
use ratatui::layout::{Position, Rect};
use uuid::Uuid;

use crate::protocol::{
    SessionTimestamp, UsageMetricsBreakdown, UsageMetricsInterval, UsageMetricsQuery,
    UsageMetricsReport,
};

use calendar::{MetricsRange, RangeBuckets};
use chart::MetricsMeasure;

pub(super) use view::render;

/// Why the view has no fresh figures to show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageMetricsReadRefusal {
    /// The Server answered, and refused the read, in its own words.
    Refused(String),
    /// The read never reached a Server that could answer it, for the reason
    /// given: an Origin that stopped answering, or a request that failed.
    Failed(String),
}

impl UsageMetricsReadRefusal {
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
    range: MetricsRange,
    measure: MetricsMeasure,
    /// The read the view awaits, and what it asked for.
    asked: Option<(Uuid, Asked)>,
    /// The last report read, with the buckets it answers.
    shown: Option<(RangeBuckets, UsageMetricsReport)>,
    /// Why the last read gave no report.
    refusal: Option<UsageMetricsReadRefusal>,
    /// When the earliest timed Turn was, as the last report since opening
    /// said, from which All time begins.
    earliest: EarliestTurn,
    /// Where the last frame drew what a pointer can act on.
    targets: Cell<Targets>,
}

/// What a read the view awaits asked for.
#[derive(Clone, Debug)]
enum Asked {
    /// No buckets at all, only when the earliest timed Turn was, which All
    /// time must learn before it can cut its own.
    EarliestTurn,
    /// The range's buckets, as the calendar cut them.
    Range(RangeBuckets),
}

/// What the view knows of the earliest timed Turn on the Server.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EarliestTurn {
    /// No report since the view opened has said.
    #[default]
    Unknown,
    /// A report said no Turn is timed.
    Untimed,
    At(SessionTimestamp),
}

impl From<Option<SessionTimestamp>> for EarliestTurn {
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
    /// Each range's place in the range bar, in [`MetricsRange::ALL`] order.
    ranges: [Option<Rect>; MetricsRange::ALL.len()],
    /// Each measure's place beside it, in [`MetricsMeasure::ALL`] order.
    measures: [Option<Rect>; MetricsMeasure::ALL.len()],
    /// The chart, or where it stands while there is none to draw.
    chart: Option<Rect>,
}

/// A part of the view a pointer can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetricsTarget {
    Close,
    Range(MetricsRange),
    Measure(MetricsMeasure),
    Chart,
}

/// What the view shows beneath its title bar.
pub(super) enum MetricsContent<'a> {
    Reading,
    Refused(&'a UsageMetricsReadRefusal),
    Report {
        buckets: &'a RangeBuckets,
        report: &'a UsageMetricsReport,
    },
}

impl MetricsView {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the view with nothing yet read, keeping the choices it had.
    pub(super) fn open(&mut self) {
        self.open = true;
        self.asked = None;
        self.shown = None;
        self.refusal = None;
        self.earliest = EarliestTurn::Unknown;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.asked = None;
        self.targets.set(Targets::default());
    }

    /// Chooses the range the view shows, answering whether that changed it,
    /// so that it must be read anew. Figures for another range are not shown
    /// beside the new range's name while they are.
    pub(super) fn choose_range(&mut self, range: MetricsRange) -> bool {
        if self.range == range {
            return false;
        }
        self.range = range;
        self.shown = None;
        self.refusal = None;
        true
    }

    /// The query for the view's range at `now` in `zone`, which the view now
    /// awaits the answer to under the request it answers with. A report
    /// already shown stays until the answer replaces it. A moment the
    /// calendar cannot place is refused in the view instead.
    ///
    /// All time begins with the earliest timed Turn's month, so before any
    /// report has said when that was, it asks for no buckets at all: a read
    /// that answers with nothing but the earliest Turn, from which
    /// [`Self::receive`] has the view read again.
    pub(super) fn ask(
        &mut self,
        now: SessionTimestamp,
        zone: &TimeZone,
    ) -> Option<(Uuid, UsageMetricsQuery)> {
        let earliest = match self.earliest {
            EarliestTurn::Unknown if self.range == MetricsRange::AllTime => {
                return Some(self.await_answer(Asked::EarliestTurn, Vec::new()));
            }
            EarliestTurn::At(earliest) => Some(earliest),
            EarliestTurn::Unknown | EarliestTurn::Untimed => None,
        };
        let Some(buckets) = calendar::resolve(self.range, now, zone, earliest) else {
            self.refuse(UsageMetricsReadRefusal::Failed(
                "The clock reads a moment the calendar cannot place".to_owned(),
            ));
            return None;
        };
        let intervals = buckets
            .buckets
            .iter()
            .map(|bucket| bucket.interval)
            .collect();
        Some(self.await_answer(Asked::Range(buckets), intervals))
    }

    fn await_answer(
        &mut self,
        asked: Asked,
        buckets: Vec<UsageMetricsInterval>,
    ) -> (Uuid, UsageMetricsQuery) {
        let request_id = Uuid::new_v4();
        let query = UsageMetricsQuery {
            buckets,
            breakdown: UsageMetricsBreakdown::Model,
        };
        self.asked = Some((request_id, asked));
        self.refusal = None;
        (request_id, query)
    }

    /// Takes the Server's answer, unless the view has since closed or asked
    /// again, and says whether the view must read again: after All time has
    /// learned when the earliest Turn was, or of a Turn earlier than the
    /// buckets it asked for begin.
    pub(super) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<UsageMetricsReport, UsageMetricsReadRefusal>,
    ) -> Receipt {
        if !self
            .asked
            .as_ref()
            .is_some_and(|(asked, _)| *asked == request_id)
        {
            return Receipt::Settled;
        }
        let (_, asked) = self.asked.take().expect("the awaited read");
        let report = match result {
            Ok(report) => report,
            Err(refusal) => {
                self.refuse(refusal);
                return Receipt::Settled;
            }
        };
        let earliest = EarliestTurn::from(report.earliest_turn_at);
        // Asked again only for an earliest Turn the cut did not begin from,
        // so a read whose buckets cannot reach back that far is asked once.
        let learned = self.earliest != earliest;
        self.earliest = earliest;
        let buckets = match asked {
            Asked::EarliestTurn => return Receipt::ReadAgain,
            Asked::Range(buckets) => buckets,
        };
        let uncovered = report.earliest_turn_at.is_some_and(|earliest| {
            buckets
                .buckets
                .first()
                .is_none_or(|first| earliest < first.interval.start)
        });
        if self.range == MetricsRange::AllTime && learned && uncovered {
            return Receipt::ReadAgain;
        }
        self.shown = Some((buckets, report));
        self.refusal = None;
        Receipt::Settled
    }

    /// Says why the view has no fresh figures, giving up any read it
    /// awaits. A report already shown stays, with the refusal beside it.
    pub(super) fn refuse(&mut self, refusal: UsageMetricsReadRefusal) {
        self.asked = None;
        self.refusal = Some(refusal);
    }

    /// Why the figures shown could not be read again, where they could not.
    pub(super) fn stale_because(&self) -> Option<&UsageMetricsReadRefusal> {
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

    pub(super) fn content(&self) -> MetricsContent<'_> {
        match (&self.shown, &self.refusal) {
            (Some((buckets, report)), _) => MetricsContent::Report { buckets, report },
            (None, Some(refusal)) => MetricsContent::Refused(refusal),
            (None, None) => MetricsContent::Reading,
        }
    }

    /// Forgets where the last frame drew anything, before the next draws.
    pub(super) fn forget_targets(&self) {
        self.targets.set(Targets::default());
    }

    pub(super) fn record_close_target(&self, area: Rect) {
        self.update_targets(|targets| targets.close = Some(area));
    }

    pub(super) fn record_range_target(&self, range: MetricsRange, area: Rect) {
        self.update_targets(|targets| targets.ranges[range.place()] = Some(area));
    }

    pub(super) fn record_measure_target(&self, measure: MetricsMeasure, area: Rect) {
        self.update_targets(|targets| targets.measures[measure.place()] = Some(area));
    }

    pub(super) fn record_chart_target(&self, area: Rect) {
        self.update_targets(|targets| targets.chart = Some(area));
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
        if let Some(range) = MetricsRange::ALL
            .into_iter()
            .find(|range| on(targets.ranges[range.place()]))
        {
            return Some(MetricsTarget::Range(range));
        }
        if let Some(measure) = MetricsMeasure::ALL
            .into_iter()
            .find(|measure| on(targets.measures[measure.place()]))
        {
            return Some(MetricsTarget::Measure(measure));
        }
        on(targets.chart).then_some(MetricsTarget::Chart)
    }
}
