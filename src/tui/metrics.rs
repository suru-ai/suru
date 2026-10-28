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
    SessionTimestamp, UsageMetricsBreakdown, UsageMetricsQuery, UsageMetricsReport,
};

use calendar::{BucketUnit, MetricsRange, RangeBuckets};
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
    /// The read the view awaits, and the buckets it asked for.
    asked: Option<(Uuid, RangeBuckets)>,
    /// The last report read, with the buckets it answers.
    shown: Option<(RangeBuckets, UsageMetricsReport)>,
    /// Why the last read gave no report.
    refusal: Option<UsageMetricsReadRefusal>,
    /// When the earliest timed Turn was, as the last report since opening
    /// said — `Some(None)` where it said none is timed — from which All time
    /// begins. `None` until a report says, so All time reads once first to
    /// learn it.
    earliest: Option<Option<SessionTimestamp>>,
    /// Where the last frame drew what a pointer can act on.
    targets: Cell<Targets>,
}

/// Where the last frame drew each part of the view a pointer can act on.
#[derive(Clone, Copy, Debug, Default)]
struct Targets {
    close: Option<Rect>,
    /// Each range's place in the range bar, in [`MetricsRange::ALL`] order.
    ranges: [Option<Rect>; MetricsRange::ALL.len()],
    /// The chart, or where it stands while there is none to draw.
    chart: Option<Rect>,
}

/// A part of the view a pointer can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetricsTarget {
    Close,
    Range(MetricsRange),
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
        self.earliest = None;
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
        let buckets = match (self.range, self.earliest) {
            (MetricsRange::AllTime, None) => Some(RangeBuckets {
                unit: BucketUnit::Month,
                buckets: Vec::new(),
            }),
            (range, earliest) => calendar::resolve(range, now, zone, earliest.flatten()),
        };
        let Some(buckets) = buckets else {
            self.refuse(UsageMetricsReadRefusal::Failed(
                "The clock reads a moment the calendar cannot place".to_owned(),
            ));
            return None;
        };
        let request_id = Uuid::new_v4();
        let query = UsageMetricsQuery {
            buckets: buckets
                .buckets
                .iter()
                .map(|bucket| bucket.interval)
                .collect(),
            breakdown: UsageMetricsBreakdown::Model,
        };
        self.asked = Some((request_id, buckets));
        self.refusal = None;
        Some((request_id, query))
    }

    /// Takes the Server's answer, unless the view has since closed or asked
    /// again, and answers whether the view must read again: when All time
    /// learns of a Turn earlier than the buckets it asked for begin, which
    /// its first read, asking for none, always does where any Turn is timed.
    pub(super) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<UsageMetricsReport, UsageMetricsReadRefusal>,
    ) -> bool {
        if !self
            .asked
            .as_ref()
            .is_some_and(|(asked, _)| *asked == request_id)
        {
            return false;
        }
        let (_, buckets) = self.asked.take().expect("the awaited read");
        let report = match result {
            Ok(report) => report,
            Err(refusal) => {
                self.refuse(refusal);
                return false;
            }
        };
        // Asked again only for an earliest Turn the cut did not begin from,
        // so a read whose buckets cannot reach back that far is asked once.
        let learned = self.earliest != Some(report.earliest_turn_at);
        self.earliest = Some(report.earliest_turn_at);
        let uncovered = report.earliest_turn_at.is_some_and(|earliest| {
            buckets
                .buckets
                .first()
                .is_none_or(|first| earliest < first.interval.start)
        });
        if self.range == MetricsRange::AllTime && learned && uncovered {
            return true;
        }
        self.shown = Some((buckets, report));
        self.refusal = None;
        false
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
        on(targets.chart).then_some(MetricsTarget::Chart)
    }
}
