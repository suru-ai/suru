//! The Usage Metrics calendar: where a range's buckets begin and end, cut at
//! the reader's own local boundaries. The Server knows nothing of calendars
//! or time zones, so the reader resolves them here, a pure function of the
//! range, the moment now, and the zone, and asks for the intervals.

use jiff::{Timestamp, ToSpan, Zoned, civil::DateTime, tz::TimeZone};

use crate::protocol::{SessionTimestamp, UsageMetricsInterval};

/// The span of time a Usage Metrics view covers, ending with the present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MetricsRange {
    /// Today and the 29 local days before it, a bucket each.
    #[default]
    ThirtyDays,
}

impl MetricsRange {
    /// How the range is named where it is shown.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ThirtyDays => "Last 30 days",
        }
    }

    /// The unit one bucket of the range is.
    pub(crate) fn unit(self) -> BucketUnit {
        match self {
            Self::ThirtyDays => BucketUnit::Day,
        }
    }
}

/// What one bucket of a range is, which says how it is labelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BucketUnit {
    Day,
}

impl BucketUnit {
    /// The spans of buckets an x-axis may label every one of, shortest
    /// first: a reader counts days by the week.
    pub(crate) fn label_strides(self) -> &'static [u16] {
        match self {
            Self::Day => &[1, 7, 14, 28],
        }
    }
}

/// One bucket as the reader's calendar cuts it: the interval asked of the
/// Server, and the local moment it begins at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CalendarBucket {
    pub(crate) interval: UsageMetricsInterval,
    pub(crate) begins: DateTime,
}

/// The buckets `range` divides into at `now` in `zone`, earliest first and
/// each beginning where the one before it ends. `None` where `now` lies
/// beyond what the calendar can place.
pub(crate) fn resolve(
    range: MetricsRange,
    now: SessionTimestamp,
    zone: &TimeZone,
) -> Option<Vec<CalendarBucket>> {
    let millis = i64::try_from(now.0).ok()?;
    let now = Timestamp::from_millisecond(millis)
        .ok()?
        .to_zoned(zone.clone());
    match range {
        MetricsRange::ThirtyDays => days(&now, 30),
    }
}

/// Today and the `count - 1` local days before it. A day is cut at its
/// first moment rather than at midnight, which a transition can skip, so a
/// 23- or 25-hour day is still one bucket.
fn days(now: &Zoned, count: i64) -> Option<Vec<CalendarBucket>> {
    let today = now.start_of_day().ok()?;
    (0..count)
        .rev()
        .map(|back| {
            let begins = today.checked_sub(back.days()).ok()?.start_of_day().ok()?;
            let ends = begins.tomorrow().ok()?.start_of_day().ok()?;
            bucket(&begins, &ends)
        })
        .collect()
}

fn bucket(begins: &Zoned, ends: &Zoned) -> Option<CalendarBucket> {
    let moment = |zoned: &Zoned| {
        u64::try_from(zoned.timestamp().as_millisecond())
            .ok()
            .map(SessionTimestamp)
    };
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

    #[test]
    fn thirty_days_are_today_and_the_twenty_nine_local_days_before_it() {
        let now = at("Asia/Kolkata", date(2026, 10, 28).at(0, 30, 0, 0));

        let buckets = resolve(MetricsRange::ThirtyDays, now, &zone("Asia/Kolkata")).unwrap();

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
        assert!(
            buckets
                .windows(2)
                .all(|pair| pair[0].interval.end == pair[1].interval.start),
            "each day ends where the next begins"
        );
    }

    #[test]
    fn the_same_instant_falls_on_the_reader_s_own_day() {
        // 03:00 UTC on 28 October is still the 27th in New York.
        let now = at("UTC", date(2026, 10, 28).at(3, 0, 0, 0));

        let new_york = resolve(MetricsRange::ThirtyDays, now, &zone("America/New_York")).unwrap();
        let kolkata = resolve(MetricsRange::ThirtyDays, now, &zone("Asia/Kolkata")).unwrap();

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

        let buckets = resolve(MetricsRange::ThirtyDays, now, &zone("America/New_York")).unwrap();

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
        let buckets = resolve(MetricsRange::ThirtyDays, now, &zone("America/New_York")).unwrap();
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

        let buckets = resolve(MetricsRange::ThirtyDays, now, &zone("America/Santiago")).unwrap();

        let skipped = buckets
            .iter()
            .find(|bucket| bucket.begins.date() == date(2026, 9, 6))
            .expect("6 September is in range");
        assert_eq!(skipped.begins, date(2026, 9, 6).at(1, 0, 0, 0));
        assert_eq!(hours(skipped.interval), 23);
        assert!(
            buckets
                .windows(2)
                .all(|pair| pair[0].interval.end == pair[1].interval.start)
        );
    }
}
