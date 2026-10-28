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

use calendar::{CalendarBucket, MetricsRange};
use chart::MetricsMeasure;

pub(super) use view::render;

/// The view's state. Its choices outlive its closing, for the rest of the
/// TUI process; what it read does not, since the next opening reads again.
#[derive(Clone, Debug, Default)]
pub(super) struct MetricsView {
    open: bool,
    range: MetricsRange,
    measure: MetricsMeasure,
    /// The read the view awaits, and the buckets it asked for.
    asked: Option<(Uuid, Vec<CalendarBucket>)>,
    /// The last report read, with the buckets it answers.
    shown: Option<(Vec<CalendarBucket>, UsageMetricsReport)>,
    /// Why the last read gave no report.
    refusal: Option<String>,
    /// Where the last frame drew the close control.
    close_target: Cell<Option<Rect>>,
}

/// What the view shows beneath its title bar.
pub(super) enum MetricsContent<'a> {
    Reading,
    Refused(&'a str),
    Report {
        buckets: &'a [CalendarBucket],
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
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.asked = None;
        self.close_target.set(None);
    }

    /// The query for the view's range at `now` in `zone`, which the view now
    /// awaits the answer to under the request it answers with. A report
    /// already shown stays until the answer replaces it. A moment the
    /// calendar cannot place is refused in the view instead.
    pub(super) fn ask(
        &mut self,
        now: SessionTimestamp,
        zone: &TimeZone,
    ) -> Option<(Uuid, UsageMetricsQuery)> {
        let Some(buckets) = calendar::resolve(self.range, now, zone) else {
            self.refuse("The clock reads a moment the calendar cannot place".to_owned());
            return None;
        };
        let request_id = Uuid::new_v4();
        let query = UsageMetricsQuery {
            buckets: buckets.iter().map(|bucket| bucket.interval).collect(),
            breakdown: UsageMetricsBreakdown::Model,
        };
        self.asked = Some((request_id, buckets));
        self.refusal = None;
        Some((request_id, query))
    }

    /// Takes the Server's answer, unless the view has since closed or asked
    /// again.
    pub(super) fn receive(&mut self, request_id: Uuid, result: Result<UsageMetricsReport, String>) {
        if !self
            .asked
            .as_ref()
            .is_some_and(|(asked, _)| *asked == request_id)
        {
            return;
        }
        let (_, buckets) = self.asked.take().expect("the awaited read");
        match result {
            Ok(report) => self.shown = Some((buckets, report)),
            Err(reason) => self.refuse(reason),
        }
    }

    /// Says why the view has no report to show, giving up any it awaits.
    pub(super) fn refuse(&mut self, reason: String) {
        self.asked = None;
        self.shown = None;
        self.refusal = Some(reason);
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
        if let Some(reason) = &self.refusal {
            return MetricsContent::Refused(reason);
        }
        match &self.shown {
            Some((buckets, report)) => MetricsContent::Report { buckets, report },
            None => MetricsContent::Reading,
        }
    }

    pub(super) fn record_close_target(&self, area: Rect) {
        self.close_target.set(Some(area));
    }

    /// Whether `position` is on the close control the last frame drew.
    pub(super) fn close_target_contains(&self, position: Position) -> bool {
        self.close_target
            .get()
            .is_some_and(|area| area.contains(position))
    }
}
