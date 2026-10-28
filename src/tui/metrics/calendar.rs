//! The Usage Metrics calendar: where a range's buckets begin and end, cut at
//! the reader's own local boundaries, and the interval each is compared
//! with. The Server knows nothing of calendars or time zones, so the reader
//! resolves them here, a pure function of the range, the moment now, the
//! zone, and — for All time — the earliest timed Turn, and asks for the
//! intervals.

use jiff::{
    Span, Timestamp, ToSpan, Zoned,
    civil::{Date, DateTime},
    tz::TimeZone,
};

use crate::protocol::{MAX_USAGE_METRICS_BUCKETS, SessionTimestamp, UsageMetricsInterval};

/// The span of time a Usage Metrics view covers, ending with the present.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum MetricsRange {
    /// Today's local hours, a bucket each.
    Today,
    /// Today and the 6 local days before it, a bucket each.
    SevenDays,
    /// Today and the 29 local days before it, a bucket each.
    #[default]
    ThirtyDays,
    /// This calendar month and the 11 before it, a bucket each.
    Year,
    /// Every calendar month since the earliest timed Turn's, gathered into
    /// quarters past [`ALL_TIME_MONTHS`] months and into years past
    /// [`ALL_TIME_QUARTERS`] quarters' worth of them.
    AllTime,
}

/// The most months All time shows one bar each.
pub(crate) const ALL_TIME_MONTHS: i64 = 36;
/// The most months All time shows by the quarter, twelve years' worth.
pub(crate) const ALL_TIME_QUARTERS: i64 = 12 * 12;

impl MetricsRange {
    /// Every range, in the order the range bar lists them and `1`–`5` and
    /// `[`/`]` step through them.
    pub const ALL: [Self; 5] = [
        Self::Today,
        Self::SevenDays,
        Self::ThirtyDays,
        Self::Year,
        Self::AllTime,
    ];

    /// How the range bar names the range.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::SevenDays => "7 days",
            Self::ThirtyDays => "30 days",
            Self::Year => "Year",
            Self::AllTime => "All time",
        }
    }

    /// How a heading names the span the range covers.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::SevenDays => "Last 7 days",
            Self::ThirtyDays => "Last 30 days",
            Self::Year => "Last 12 months",
            Self::AllTime => "All time",
        }
    }

    /// How long the range is in the reader's calendar, ending with the
    /// present: so many local days or calendar months. All time has no
    /// length of its own; it runs from the earliest timed Turn.
    pub(crate) fn length(self) -> Option<RangeLength> {
        match self {
            Self::Today => Some(RangeLength::Days(1)),
            Self::SevenDays => Some(RangeLength::Days(7)),
            Self::ThirtyDays => Some(RangeLength::Days(30)),
            Self::Year => Some(RangeLength::Months(12)),
            Self::AllTime => None,
        }
    }

    /// How a comparison names the period the range is compared with, where
    /// it is compared with any.
    pub(crate) fn compared_with(self) -> Option<String> {
        Some(match self.length()? {
            RangeLength::Days(1) => "vs yesterday".to_owned(),
            RangeLength::Days(days) => format!("vs prev {days}d"),
            RangeLength::Months(months) => format!("vs prev {months}mo"),
        })
    }

    /// What the view says of the range where it holds no Usage.
    pub(crate) fn no_usage(self) -> &'static str {
        match self {
            Self::Today => "No Usage today.",
            Self::SevenDays => "No Usage in the last 7 days.",
            Self::ThirtyDays => "No Usage in the last 30 days.",
            Self::Year => "No Usage in the last 12 months.",
            Self::AllTime => "No Usage yet.",
        }
    }

    /// The range after this one in the range bar, or this one where it is
    /// the last: the wheel stops at either end rather than spinning round.
    pub(crate) fn next(self) -> Self {
        let place = self.place();
        Self::ALL[(place + 1).min(Self::ALL.len() - 1)]
    }

    /// The range before this one in the range bar, or this one where it is
    /// the first.
    pub(crate) fn previous(self) -> Self {
        Self::ALL[self.place().saturating_sub(1)]
    }

    /// Where the range stands in the range bar, from nought.
    pub(crate) fn place(self) -> usize {
        Self::ALL
            .iter()
            .position(|range| *range == self)
            .expect("every range is listed")
    }
}

/// How long a range is: so many local days, a bucket each but the single
/// day, which is cut by the hour, or so many calendar months, a bucket each.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RangeLength {
    Days(i64),
    Months(i64),
}

impl RangeLength {
    fn span(self) -> Span {
        match self {
            Self::Days(days) => days.days(),
            Self::Months(months) => months.months(),
        }
    }
}

/// What one bucket of a range is, which says how it is labelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BucketUnit {
    Hour,
    Day,
    Month,
    Quarter,
    Year,
}

impl BucketUnit {
    /// The spans of buckets an x-axis may label every one of, shortest
    /// first, in the multiples a reader counts the unit in: hours by the
    /// quarter- and half-day, days by the week, months by the quarter, half
    /// year, and year, years by the decade.
    pub(crate) fn label_strides(self) -> &'static [u16] {
        match self {
            Self::Hour => &[1, 2, 3, 6, 12],
            Self::Day => &[1, 7, 14, 28],
            Self::Month => &[1, 3, 6, 12],
            Self::Quarter => &[1, 2, 4],
            Self::Year => &[1, 2, 5, 10],
        }
    }
}

/// A range as the reader's calendar cuts it: the unit of its buckets, the
/// buckets, earliest first and each beginning where the one before it ends,
/// and the interval its figures are compared with, where any comes before.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RangeBuckets {
    pub(crate) unit: BucketUnit,
    pub(crate) buckets: Vec<CalendarBucket>,
    /// The range moved back by its own length, up to the moment as far into
    /// it as now is into the range: see [`resolve`].
    pub(crate) comparison: Option<UsageMetricsInterval>,
}

/// One bucket as the reader's calendar cuts it: the interval asked of the
/// Server, and the local moment it begins at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CalendarBucket {
    pub(crate) interval: UsageMetricsInterval,
    pub(crate) begins: DateTime,
}

/// The buckets `range` divides into at `now` in `zone`. All time begins
/// with the month of `earliest`, the earliest timed Turn, and is this month
/// alone where no Turn is timed; no other range consults it. `None` where
/// `now` lies beyond what the calendar can place.
///
/// Every range but All time, before which nothing comes, is compared with
/// the period of the same length before it, in the reader's calendar: as
/// many local days or calendar months back from its first, up to the same
/// reading of the clock as many days or months before now. So the range so
/// far is compared with as much of the period before: Today with yesterday
/// up to this time of day, 30 days with the 30 local days before as far
/// into the last of them, and Year with the 12 months before as far into
/// the twelfth, a day past the end of a shorter month taken as its last.
pub(crate) fn resolve(
    range: MetricsRange,
    now: SessionTimestamp,
    zone: &TimeZone,
    earliest: Option<SessionTimestamp>,
) -> Option<RangeBuckets> {
    let now = zoned(now, zone)?;
    let today = now.date();
    let this_month = today.first_of_month();
    let length = range.length();
    let (unit, buckets) = match length {
        Some(RangeLength::Days(1)) => (BucketUnit::Hour, hours(&now)?),
        Some(RangeLength::Days(count)) => (BucketUnit::Day, days(today, count, zone)?),
        Some(RangeLength::Months(count)) => (
            BucketUnit::Month,
            periods(
                this_month.checked_sub((count - 1).months()).ok()?,
                count,
                1,
                zone,
            )?,
        ),
        None => {
            // A Turn the clock places after now begins no earlier month.
            let earliest = match earliest {
                Some(earliest) => zoned(earliest, zone)?.date().min(today),
                None => today,
            };
            all_time(earliest, today, zone)?
        }
    };
    let comparison = match (length, buckets.first()) {
        (Some(length), Some(first)) => Some(compared(first, &now, length.span(), zone)?),
        _ => None,
    };
    Some(RangeBuckets {
        unit,
        buckets,
        comparison,
    })
}

/// The range whose first bucket is `first` moved back by `length`, up to
/// `now` moved back by it: from the first moment of the day `length` before
/// the first's, to the same reading of the clock `length` before now — or
/// the nearest that exists, where the clock skipped it — and never before
/// it begins.
fn compared(
    first: &CalendarBucket,
    now: &Zoned,
    length: Span,
    zone: &TimeZone,
) -> Option<UsageMetricsInterval> {
    let begins = first
        .begins
        .date()
        .checked_sub(length)
        .ok()?
        .to_zoned(zone.clone())
        .ok()?
        .start_of_day()
        .ok()?;
    let ends = now.checked_sub(length).ok()?;
    let ends = if ends.timestamp() < begins.timestamp() {
        begins.clone()
    } else {
        ends
    };
    Some(UsageMetricsInterval {
        start: moment(&begins)?,
        end: moment(&ends)?,
    })
}

/// The Session timestamp of `zoned`.
fn moment(zoned: &Zoned) -> Option<SessionTimestamp> {
    u64::try_from(zoned.timestamp().as_millisecond())
        .ok()
        .map(SessionTimestamp)
}

/// From the month of `earliest` through the month of `today`: by the month
/// up to [`ALL_TIME_MONTHS`] of them, then by the calendar quarter up to
/// [`ALL_TIME_QUARTERS`] months, then by the calendar year — no more years
/// than one query may ask for, the latest kept.
fn all_time(
    earliest: Date,
    today: Date,
    zone: &TimeZone,
) -> Option<(BucketUnit, Vec<CalendarBucket>)> {
    let months = i64::from(today.year() - earliest.year()) * 12
        + i64::from(today.month() - earliest.month())
        + 1;
    if months <= ALL_TIME_MONTHS {
        let buckets = periods(earliest.first_of_month(), months, 1, zone)?;
        return Some((BucketUnit::Month, buckets));
    }
    if months <= ALL_TIME_QUARTERS {
        let quarter = |date: Date| {
            date.first_of_month()
                .with()
                .month((date.month() - 1) / 3 * 3 + 1)
                .build()
                .ok()
        };
        let first = quarter(earliest)?;
        let last = quarter(today)?;
        let count = i64::from(last.year() - first.year()) * 4
            + i64::from(last.month() - first.month()) / 3
            + 1;
        return Some((BucketUnit::Quarter, periods(first, count, 3, zone)?));
    }
    let most = i16::try_from(MAX_USAGE_METRICS_BUCKETS).unwrap_or(i16::MAX);
    let first = earliest.year().max(today.year() - (most - 1));
    let count = i64::from(today.year() - first) + 1;
    let first = today.first_of_year().with().year(first).build().ok()?;
    Some((BucketUnit::Year, periods(first, count, 12, zone)?))
}

fn zoned(moment: SessionTimestamp, zone: &TimeZone) -> Option<Zoned> {
    let millis = i64::try_from(moment.0).ok()?;
    Some(
        Timestamp::from_millisecond(millis)
            .ok()?
            .to_zoned(zone.clone()),
    )
}

/// Today's local hours, from its first moment to tomorrow's: 23 or 25 of
/// them on a day daylight saving shortens or lengthens.
fn hours(now: &Zoned) -> Option<Vec<CalendarBucket>> {
    let begins = now.start_of_day().ok()?;
    let ends = begins.tomorrow().ok()?.start_of_day().ok()?;
    let mut edges = vec![begins];
    loop {
        let last = edges.last()?;
        if last.timestamp() >= ends.timestamp() {
            break;
        }
        // An hour of elapsed time, so a repeated hour is a bucket of its own.
        let next = last.checked_add(1.hour()).ok()?;
        edges.push(if next.timestamp() < ends.timestamp() {
            next
        } else {
            ends.clone()
        });
    }
    between(&edges)
}

/// `today` and the `count - 1` local days before it.
fn days(today: Date, count: i64, zone: &TimeZone) -> Option<Vec<CalendarBucket>> {
    let first = today.checked_sub((count - 1).days()).ok()?;
    edges(first, count, zone, |step| step.days())
}

/// `count` calendar periods of `months` months each, the first beginning on
/// `first`.
fn periods(first: Date, count: i64, months: i64, zone: &TimeZone) -> Option<Vec<CalendarBucket>> {
    edges(first, count, zone, |step| (step * months).months())
}

/// `count` buckets, the `n`th beginning `step(n)` after `first`. Each is cut
/// at its first day's first moment rather than at midnight, which a
/// transition can skip, so a 23- or 25-hour day is still one bucket.
fn edges(
    first: Date,
    count: i64,
    zone: &TimeZone,
    step: impl Fn(i64) -> Span,
) -> Option<Vec<CalendarBucket>> {
    let edges = (0..=count)
        .map(|n| {
            first
                .checked_add(step(n))
                .ok()?
                .to_zoned(zone.clone())
                .ok()?
                .start_of_day()
                .ok()
        })
        .collect::<Option<Vec<Zoned>>>()?;
    between(&edges)
}

/// The buckets between each edge and the next.
fn between(edges: &[Zoned]) -> Option<Vec<CalendarBucket>> {
    edges
        .windows(2)
        .map(|pair| bucket(&pair[0], &pair[1]))
        .collect()
}

fn bucket(begins: &Zoned, ends: &Zoned) -> Option<CalendarBucket> {
    Some(CalendarBucket {
        interval: UsageMetricsInterval {
            start: moment(begins)?,
            end: moment(ends)?,
        },
        begins: begins.datetime(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap_or_else(|error| panic!("the zone {name}: {error}"))
    }

    /// The moment a wall-clock reading in `zone` names.
    fn at(zone_name: &str, wall: DateTime) -> SessionTimestamp {
        let zoned = wall
            .to_zoned(zone(zone_name))
            .expect("a wall-clock reading");
        SessionTimestamp(zoned.timestamp().as_millisecond() as u64)
    }

    fn hours(interval: UsageMetricsInterval) -> u64 {
        (interval.end.0 - interval.start.0) / 3_600_000
    }

    /// The buckets `range` divides into at `now` in the zone named, for a
    /// range that does not consult the earliest Turn.
    fn cut(range: MetricsRange, now: SessionTimestamp, zone_name: &str) -> Vec<CalendarBucket> {
        resolve(range, now, &zone(zone_name), None)
            .expect("a moment the calendar places")
            .buckets
    }

    /// All time at `now` in UTC since a Turn at `earliest` there.
    fn all_time_since(earliest: DateTime, now: DateTime) -> RangeBuckets {
        resolve(
            MetricsRange::AllTime,
            at("UTC", now),
            &zone("UTC"),
            Some(at("UTC", earliest)),
        )
        .expect("moments the calendar places")
    }

    fn contiguous(buckets: &[CalendarBucket]) -> bool {
        buckets
            .windows(2)
            .all(|pair| pair[0].interval.end == pair[1].interval.start)
    }

    fn hours_of_the_day(buckets: &[CalendarBucket]) -> Vec<i8> {
        buckets.iter().map(|bucket| bucket.begins.hour()).collect()
    }

    #[test]
    fn today_is_its_local_hours_from_midnight_to_midnight() {
        let now = at("Asia/Tokyo", date(2026, 10, 28).at(14, 30, 0, 0));

        let buckets = cut(MetricsRange::Today, now, "Asia/Tokyo");

        assert_eq!(hours_of_the_day(&buckets), (0..24).collect::<Vec<_>>());
        assert_eq!(
            buckets[0].interval.start,
            at("Asia/Tokyo", date(2026, 10, 28).at(0, 0, 0, 0))
        );
        assert_eq!(
            buckets[23].interval.end,
            at("Asia/Tokyo", date(2026, 10, 29).at(0, 0, 0, 0)),
            "the hours still to come are buckets too"
        );
        assert!(buckets[14].interval.contains(now));
        assert!(buckets.iter().all(|bucket| hours(bucket.interval) == 1));
        assert!(contiguous(&buckets));
    }

    #[test]
    fn today_has_twenty_three_hours_when_daylight_saving_begins() {
        // New York springs forward at 02:00 on 8 March 2026, to 03:00.
        let now = at("America/New_York", date(2026, 3, 8).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::Today, now, "America/New_York");

        assert_eq!(buckets.len(), 23);
        let mut wall: Vec<i8> = (0..24).collect();
        wall.retain(|hour| *hour != 2);
        assert_eq!(hours_of_the_day(&buckets), wall, "02:00 never comes");
        assert!(buckets.iter().all(|bucket| hours(bucket.interval) == 1));
        assert!(contiguous(&buckets));
        assert_eq!(
            buckets[22].interval.end,
            at("America/New_York", date(2026, 3, 9).at(0, 0, 0, 0))
        );
    }

    #[test]
    fn today_has_twenty_five_hours_when_daylight_saving_ends() {
        // New York falls back at 02:00 on 1 November 2026, to 01:00 again.
        let now = at("America/New_York", date(2026, 11, 1).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::Today, now, "America/New_York");

        assert_eq!(buckets.len(), 25);
        let mut wall: Vec<i8> = (0..24).collect();
        wall.insert(1, 1);
        assert_eq!(
            hours_of_the_day(&buckets),
            wall,
            "01:00 comes twice, a bucket each time"
        );
        assert!(buckets.iter().all(|bucket| hours(bucket.interval) == 1));
        assert!(contiguous(&buckets));
    }

    #[test]
    fn today_begins_at_its_first_moment_where_midnight_is_skipped() {
        // Santiago springs forward at midnight on 6 September 2026.
        let now = at("America/Santiago", date(2026, 9, 6).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::Today, now, "America/Santiago");

        assert_eq!(buckets.len(), 23);
        assert_eq!(buckets[0].begins, date(2026, 9, 6).at(1, 0, 0, 0));
        assert_eq!(hours_of_the_day(&buckets), (1..24).collect::<Vec<_>>());
    }

    #[test]
    fn today_keeps_whole_hours_through_a_half_hour_transition() {
        // Lord Howe Island springs forward half an hour at 02:00 on
        // 4 October 2026, so its day is 23½ hours long.
        let now = at("Australia/Lord_Howe", date(2026, 10, 4).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::Today, now, "Australia/Lord_Howe");

        assert_eq!(buckets.len(), 24);
        assert_eq!(buckets[2].begins, date(2026, 10, 4).at(2, 30, 0, 0));
        assert!(
            buckets[..23]
                .iter()
                .all(|bucket| hours(bucket.interval) == 1)
        );
        let last = buckets[23].interval;
        assert_eq!(
            last.end.0 - last.start.0,
            30 * 60 * 1_000,
            "the last half hour"
        );
        assert_eq!(
            last.end,
            at("Australia/Lord_Howe", date(2026, 10, 5).at(0, 0, 0, 0))
        );
    }

    #[test]
    fn seven_days_are_today_and_the_six_local_days_before_it() {
        let now = at("Europe/London", date(2026, 10, 28).at(9, 0, 0, 0));

        let buckets = cut(MetricsRange::SevenDays, now, "Europe/London");

        let days: Vec<Date> = buckets.iter().map(|bucket| bucket.begins.date()).collect();
        assert_eq!(
            days,
            (22..=28).map(|day| date(2026, 10, day)).collect::<Vec<_>>()
        );
        // London falls back on the 25th.
        assert_eq!(hours(buckets[3].interval), 25);
        assert!(buckets[6].interval.contains(now));
        assert!(contiguous(&buckets));
    }

    #[test]
    fn a_year_is_this_month_and_the_eleven_before_it() {
        let now = at("Europe/London", date(2026, 10, 28).at(9, 0, 0, 0));

        let buckets = cut(MetricsRange::Year, now, "Europe/London");

        let months: Vec<Date> = buckets.iter().map(|bucket| bucket.begins.date()).collect();
        let mut expected = vec![date(2025, 11, 1), date(2025, 12, 1)];
        expected.extend((1..=10).map(|month| date(2026, month, 1)));
        assert_eq!(months, expected);
        assert!(buckets.iter().all(|bucket| bucket.begins.hour() == 0));
        assert!(buckets[11].interval.contains(now));
        assert!(contiguous(&buckets));
        let hours_in = |month: i8| {
            hours(
                buckets
                    .iter()
                    .find(|bucket| bucket.begins.date() == date(2026, month, 1))
                    .unwrap()
                    .interval,
            )
        };
        assert_eq!(hours_in(2), 28 * 24, "February 2026 is no leap month");
        assert_eq!(hours_in(3), 31 * 24 - 1, "London springs forward in March");
        assert_eq!(hours_in(10), 31 * 24 + 1, "and falls back in October");
        assert_eq!(hours_in(4), 30 * 24);
    }

    #[test]
    fn a_leap_year_s_february_is_twenty_nine_days() {
        let now = at("Asia/Kolkata", date(2028, 3, 1).at(0, 0, 0, 0));

        let buckets = cut(MetricsRange::Year, now, "Asia/Kolkata");

        assert_eq!(buckets[0].begins.date(), date(2027, 4, 1));
        let february = &buckets[10];
        assert_eq!(february.begins.date(), date(2028, 2, 1));
        assert_eq!(hours(february.interval), 29 * 24);
        assert_eq!(
            buckets[11].interval.start,
            at("Asia/Kolkata", date(2028, 3, 1).at(0, 0, 0, 0)),
            "March begins at the moment now, its first"
        );
        assert!(buckets[11].interval.contains(now));
    }

    #[test]
    fn all_time_is_monthly_from_the_earliest_turn_s_month_up_to_three_years() {
        let now = date(2026, 10, 28).at(9, 0, 0, 0);

        let cut = all_time_since(date(2023, 11, 30).at(23, 0, 0, 0), now);

        assert_eq!(cut.unit, BucketUnit::Month);
        assert_eq!(cut.buckets.len(), 36);
        assert_eq!(cut.buckets[0].begins, date(2023, 11, 1).at(0, 0, 0, 0));
        assert_eq!(cut.buckets[35].begins, date(2026, 10, 1).at(0, 0, 0, 0));
        assert!(contiguous(&cut.buckets));
    }

    #[test]
    fn all_time_past_three_years_is_by_the_calendar_quarter() {
        let now = date(2026, 10, 28).at(9, 0, 0, 0);

        let cut = all_time_since(date(2023, 10, 31).at(0, 0, 0, 0), now);

        assert_eq!(cut.unit, BucketUnit::Quarter, "37 months");
        assert_eq!(cut.buckets.len(), 13);
        assert_eq!(cut.buckets[0].begins.date(), date(2023, 10, 1));
        assert_eq!(cut.buckets[1].begins.date(), date(2024, 1, 1));
        assert_eq!(cut.buckets[12].begins.date(), date(2026, 10, 1));
        assert_eq!(
            cut.buckets[12].interval.end,
            at("UTC", date(2027, 1, 1).at(0, 0, 0, 0))
        );
        assert!(contiguous(&cut.buckets));

        // A quarter begins on the first of its first month, wherever in it
        // the earliest Turn fell.
        let cut = all_time_since(date(2014, 11, 15).at(0, 0, 0, 0), now);
        assert_eq!(cut.unit, BucketUnit::Quarter, "twelve years exactly");
        assert_eq!(cut.buckets.len(), 49);
        assert_eq!(cut.buckets[0].begins.date(), date(2014, 10, 1));
    }

    #[test]
    fn all_time_past_twelve_years_is_by_the_calendar_year() {
        let now = date(2026, 10, 28).at(9, 0, 0, 0);

        let cut = all_time_since(date(2014, 10, 31).at(0, 0, 0, 0), now);

        assert_eq!(cut.unit, BucketUnit::Year, "145 months");
        let years: Vec<Date> = cut
            .buckets
            .iter()
            .map(|bucket| bucket.begins.date())
            .collect();
        assert_eq!(
            years,
            (2014..=2026)
                .map(|year| date(year, 1, 1))
                .collect::<Vec<_>>()
        );
        assert!(contiguous(&cut.buckets));
    }

    #[test]
    fn all_time_never_asks_for_more_buckets_than_a_query_may_hold() {
        let cut = all_time_since(
            date(1970, 1, 1).at(0, 0, 0, 0),
            date(9000, 6, 1).at(0, 0, 0, 0),
        );

        assert_eq!(cut.unit, BucketUnit::Year);
        assert_eq!(cut.buckets.len(), MAX_USAGE_METRICS_BUCKETS);
        assert_eq!(cut.buckets.last().unwrap().begins.date(), date(9000, 1, 1));
    }

    #[test]
    fn all_time_without_a_timed_turn_is_this_month_alone() {
        let now = at("UTC", date(2026, 10, 28).at(9, 0, 0, 0));
        let only_this_month = |earliest| {
            let cut = resolve(MetricsRange::AllTime, now, &zone("UTC"), earliest).unwrap();
            assert_eq!(cut.unit, BucketUnit::Month);
            cut.buckets
                .iter()
                .map(|bucket| bucket.begins.date())
                .collect::<Vec<_>>()
        };

        assert_eq!(only_this_month(None), vec![date(2026, 10, 1)]);
        assert_eq!(
            only_this_month(Some(at("UTC", date(2027, 2, 1).at(0, 0, 0, 0)))),
            vec![date(2026, 10, 1)],
            "a Turn the clock places later begins no later month"
        );
    }

    #[test]
    fn all_time_begins_with_the_month_the_earliest_turn_was_in_where_the_reader_is() {
        // 02:00 UTC on 1 October is still 30 September in New York.
        let earliest = at("UTC", date(2026, 10, 1).at(2, 0, 0, 0));
        let now = at("UTC", date(2026, 10, 28).at(9, 0, 0, 0));
        let first = |zone_name: &str| {
            resolve(MetricsRange::AllTime, now, &zone(zone_name), Some(earliest))
                .unwrap()
                .buckets[0]
                .begins
                .date()
        };

        assert_eq!(first("America/New_York"), date(2026, 9, 1));
        assert_eq!(first("Europe/London"), date(2026, 10, 1));
    }

    #[test]
    fn ranges_step_in_the_range_bar_s_order_and_stop_at_either_end() {
        assert_eq!(MetricsRange::Today.previous(), MetricsRange::Today);
        assert_eq!(MetricsRange::Today.next(), MetricsRange::SevenDays);
        assert_eq!(MetricsRange::ThirtyDays.next(), MetricsRange::Year);
        assert_eq!(MetricsRange::Year.previous(), MetricsRange::ThirtyDays);
        assert_eq!(MetricsRange::AllTime.next(), MetricsRange::AllTime);
    }

    #[test]
    fn thirty_days_are_today_and_the_twenty_nine_local_days_before_it() {
        let now = at("Asia/Kolkata", date(2026, 10, 28).at(0, 30, 0, 0));

        let buckets = cut(MetricsRange::ThirtyDays, now, "Asia/Kolkata");

        assert_eq!(buckets.len(), 30);
        assert_eq!(buckets[0].begins, date(2026, 9, 29).at(0, 0, 0, 0));
        assert_eq!(buckets[29].begins, date(2026, 10, 28).at(0, 0, 0, 0));
        for (bucket, day) in buckets.iter().zip(0..) {
            let midnight = date(2026, 9, 29).checked_add(day.days()).unwrap();
            assert_eq!(
                bucket.interval.start,
                at("Asia/Kolkata", midnight.at(0, 0, 0, 0)),
                "day {day} begins at its local midnight"
            );
        }
        assert!(buckets[29].interval.contains(now));
        assert!(contiguous(&buckets), "each day ends where the next begins");
    }

    #[test]
    fn the_same_instant_falls_on_the_reader_s_own_day() {
        // 03:00 UTC on 28 October is still the 27th in New York.
        let now = at("UTC", date(2026, 10, 28).at(3, 0, 0, 0));

        let new_york = cut(MetricsRange::ThirtyDays, now, "America/New_York");
        let kolkata = cut(MetricsRange::ThirtyDays, now, "Asia/Kolkata");

        assert_eq!(new_york[29].begins.date(), date(2026, 10, 27));
        assert_eq!(kolkata[29].begins.date(), date(2026, 10, 28));
        assert_eq!(
            kolkata[29].interval.start,
            at("UTC", date(2026, 10, 27).at(18, 30, 0, 0))
        );
    }

    #[test]
    fn a_day_daylight_saving_shortens_or_lengthens_is_still_one_bucket() {
        let now = at("America/New_York", date(2026, 11, 20).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::ThirtyDays, now, "America/New_York");

        let ends = buckets
            .iter()
            .find(|bucket| bucket.begins.date() == date(2026, 11, 1))
            .expect("1 November, when New York's daylight saving ends, is in range");
        assert_eq!(hours(ends.interval), 25);
        assert!(
            buckets
                .iter()
                .filter(|bucket| bucket.begins.date() != date(2026, 11, 1))
                .all(|bucket| hours(bucket.interval) == 24)
        );

        let now = at("America/New_York", date(2026, 3, 20).at(12, 0, 0, 0));
        let buckets = cut(MetricsRange::ThirtyDays, now, "America/New_York");
        let begins = buckets
            .iter()
            .find(|bucket| bucket.begins.date() == date(2026, 3, 8))
            .expect("8 March, when New York's daylight saving begins, is in range");
        assert_eq!(hours(begins.interval), 23);
    }

    #[test]
    fn a_day_whose_midnight_is_skipped_begins_at_its_first_moment() {
        // Santiago springs forward at midnight, so 6 September 2026 has no
        // 00:00 and begins at 01:00.
        let now = at("America/Santiago", date(2026, 9, 20).at(12, 0, 0, 0));

        let buckets = cut(MetricsRange::ThirtyDays, now, "America/Santiago");

        let skipped = buckets
            .iter()
            .find(|bucket| bucket.begins.date() == date(2026, 9, 6))
            .expect("6 September is in range");
        assert_eq!(skipped.begins, date(2026, 9, 6).at(1, 0, 0, 0));
        assert_eq!(hours(skipped.interval), 23);
        assert!(contiguous(&buckets));
    }

    /// The comparison interval `range` is compared with at `now` in the zone
    /// named, as the wall-clock readings it begins and ends at there.
    fn comparison_of(
        range: MetricsRange,
        now: DateTime,
        zone_name: &str,
    ) -> Option<(SessionTimestamp, SessionTimestamp)> {
        resolve(range, at(zone_name, now), &zone(zone_name), None)
            .expect("a moment the calendar places")
            .comparison
            .map(|comparison| (comparison.start, comparison.end))
    }

    #[test]
    fn thirty_days_are_compared_with_the_thirty_local_days_before_as_far_into_them() {
        let compared = comparison_of(
            MetricsRange::ThirtyDays,
            date(2026, 10, 28).at(14, 30, 0, 0),
            "America/New_York",
        );

        // The range began on 29 September, and is fourteen and a half hours
        // into its thirtieth day.
        assert_eq!(
            compared,
            Some((
                at("America/New_York", date(2026, 8, 30).at(0, 0, 0, 0)),
                at("America/New_York", date(2026, 9, 28).at(14, 30, 0, 0)),
            ))
        );
    }

    #[test]
    fn seven_days_are_compared_with_the_seven_local_days_before() {
        assert_eq!(
            comparison_of(
                MetricsRange::SevenDays,
                date(2026, 10, 28).at(9, 0, 0, 0),
                "Europe/London",
            ),
            Some((
                at("Europe/London", date(2026, 10, 15).at(0, 0, 0, 0)),
                at("Europe/London", date(2026, 10, 21).at(9, 0, 0, 0)),
            ))
        );
    }

    #[test]
    fn today_is_compared_with_yesterday_up_to_the_same_time_of_day() {
        assert_eq!(
            comparison_of(
                MetricsRange::Today,
                date(2026, 10, 28).at(14, 30, 0, 0),
                "Asia/Tokyo",
            ),
            Some((
                at("Asia/Tokyo", date(2026, 10, 27).at(0, 0, 0, 0)),
                at("Asia/Tokyo", date(2026, 10, 27).at(14, 30, 0, 0)),
            ))
        );
        // New York falls back on 1 November: yesterday is read to the same
        // reading of the clock, not the same hours elapsed.
        assert_eq!(
            comparison_of(
                MetricsRange::Today,
                date(2026, 11, 1).at(12, 0, 0, 0),
                "America/New_York",
            ),
            Some((
                at("America/New_York", date(2026, 10, 31).at(0, 0, 0, 0)),
                at("America/New_York", date(2026, 10, 31).at(12, 0, 0, 0)),
            ))
        );
    }

    #[test]
    fn a_year_is_compared_with_the_twelve_months_before_as_far_into_them() {
        assert_eq!(
            comparison_of(
                MetricsRange::Year,
                date(2026, 10, 28).at(9, 0, 0, 0),
                "Europe/London",
            ),
            Some((
                at("Europe/London", date(2024, 11, 1).at(0, 0, 0, 0)),
                at("Europe/London", date(2025, 10, 28).at(9, 0, 0, 0)),
            ))
        );
        // A year after a leap day has none: it ends on the last day of
        // that February.
        assert_eq!(
            comparison_of(MetricsRange::Year, date(2028, 2, 29).at(9, 0, 0, 0), "UTC"),
            Some((
                at("UTC", date(2026, 3, 1).at(0, 0, 0, 0)),
                at("UTC", date(2027, 2, 28).at(9, 0, 0, 0)),
            ))
        );
    }

    #[test]
    fn all_time_is_compared_with_nothing() {
        let now = at("UTC", date(2026, 10, 28).at(9, 0, 0, 0));
        let earliest = Some(at("UTC", date(2025, 1, 15).at(0, 0, 0, 0)));

        let cut = resolve(MetricsRange::AllTime, now, &zone("UTC"), earliest).unwrap();

        assert_eq!(cut.comparison, None, "no time comes before all of it");
    }
}
