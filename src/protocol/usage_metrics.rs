//! Usage Metrics: what the Sessions living on one Server consumed over a span
//! of time, divided into time buckets the reader chooses and broken down by
//! a key. The Server knows nothing of calendars or time zones: a reader asks
//! for the buckets as intervals of Session timestamps, cut where its own
//! calendar falls, and is answered with the figures each one holds. A report
//! is read on request and never stored.

use serde::{Deserialize, Serialize};

use super::{ModelId, ProviderId, SessionTimestamp};

/// The most buckets one query may ask for: enough for every hour of a day,
/// every day of a year, or every month of decades, and few enough that a
/// report stays small.
pub const MAX_USAGE_METRICS_BUCKETS: usize = 512;

/// What a reader asks of a Server's Usage Metrics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsQuery {
    /// The time buckets, earliest first, none overlapping another and none
    /// more than [`MAX_USAGE_METRICS_BUCKETS`] of them. None at all asks only
    /// for what the report says beside its buckets: the untimed figures, and
    /// when the earliest Turn was.
    pub buckets: Vec<UsageMetricsInterval>,
    pub breakdown: UsageMetricsBreakdown,
}

/// A half-open span of time, from `start` up to but not including `end`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsInterval {
    pub start: SessionTimestamp,
    pub end: SessionTimestamp,
}

impl UsageMetricsInterval {
    pub fn contains(&self, moment: SessionTimestamp) -> bool {
        self.start <= moment && moment < self.end
    }
}

/// How each bucket's figures are split.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageMetricsBreakdown {
    /// By the Provider and Model of the Agent that ran each Turn.
    Model,
}

/// One part of a breakdown.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageMetricsKey {
    /// A Model as reached through one Provider: the same Model id under two
    /// Providers is two keys.
    Model {
        provider: ProviderId,
        model: ModelId,
    },
    /// Turns whose Agent is not known, which no other key can hold.
    Unknown,
}

/// A breakdown key and the name a reader is shown for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsKeyLabel {
    pub key: UsageMetricsKey,
    pub label: String,
}

/// A Server's answer to a [`UsageMetricsQuery`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsReport {
    /// One for each bucket asked for, in the order asked.
    pub buckets: Vec<UsageMetricsBucket>,
    /// The Turns no bucket can place, having neither begun nor Settled at a
    /// recorded moment: Turns stored before Suru recorded Turn timing.
    pub untimed: UsageMetricsBucket,
    /// The moment the earliest timed Turn on the Server is placed at, from
    /// which a reader asking for all of time can begin its buckets; absent
    /// where no Turn is timed.
    pub earliest_turn_at: Option<SessionTimestamp>,
    /// Every key a bucket breaks its figures down by, each with its label.
    pub keys: Vec<UsageMetricsKeyLabel>,
}

/// The figures of one bucket: its whole, and its part under each key a Turn
/// in the bucket ran under, whether or not that Turn recorded any Usage.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsBucket {
    pub total: UsageMetricsAggregate,
    pub by_key: Vec<KeyedUsageMetrics>,
}

/// The part of a bucket one key holds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyedUsageMetrics {
    pub key: UsageMetricsKey,
    pub aggregate: UsageMetricsAggregate,
}

/// What a set of Turns consumed together: the five parts of their Usage,
/// each absent where no Turn among them reported it, as a Session's total
/// keeps it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsAggregate {
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl UsageMetricsAggregate {
    /// Adds one Turn's Usage part by part, keeping a part absent where
    /// neither reported it, as [`super::UsageTotal::saturating_add`] does.
    pub fn add_usage(&mut self, usage: &super::Usage) {
        let add = super::add_parts;
        self.fresh_input_tokens = add(self.fresh_input_tokens, usage.fresh_input_tokens);
        self.cache_read_tokens = add(self.cache_read_tokens, usage.cache_read_tokens);
        self.cache_write_tokens = add(self.cache_write_tokens, usage.cache_write_tokens);
        self.output_tokens = add(self.output_tokens, usage.output_tokens);
        self.reasoning_tokens = add(self.reasoning_tokens, usage.reasoning_tokens);
    }

    /// The compact figure a reader is shown, on the same terms as every
    /// other surface's: see [`super::Usage::blended_tokens`].
    pub fn blended_tokens(&self) -> Option<u64> {
        super::blended_tokens(
            self.fresh_input_tokens,
            self.output_tokens,
            self.reasoning_tokens,
        )
    }
}
