//! Usage Metrics: what the Sessions living on one Server consumed over a span
//! of time, divided into time buckets the reader chooses and broken down by
//! a key. The Server knows nothing of calendars or time zones: a reader asks
//! for the buckets as intervals of Session timestamps, cut where its own
//! calendar falls, and is answered with the figures each one holds. A report
//! is read on request and never stored.

use serde::{Deserialize, Serialize};

use super::{Cost, ModelId, ProviderId, SessionId, SessionTimestamp, WorkspaceId};

/// The most buckets one query may ask for: enough for every hour of a day,
/// every day of a year, or every month of decades, and few enough that a
/// report stays small.
pub const MAX_USAGE_METRICS_BUCKETS: usize = 512;

/// The most top Sessions one query may ask for: more than any screen lists,
/// and few enough that a report stays small.
pub const MAX_USAGE_METRICS_TOP_SESSIONS: usize = 100;

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
    /// A span to compare the buckets with, whose figures the report gives
    /// taken together: the period before them, say. It stands apart from
    /// the buckets and may overlap them.
    pub comparison: Option<UsageMetricsInterval>,
    /// How many of the Sessions that cost the most across the buckets the
    /// report names, no more than [`MAX_USAGE_METRICS_TOP_SESSIONS`].
    pub top_sessions: usize,
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
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageMetricsBreakdown {
    /// By the Provider and Model of the Agent that ran each Turn.
    #[default]
    Model,
    /// By the Provider of the Agent that ran each Turn.
    Provider,
    /// By the Workspace of the Session that owns each Turn: a Subagent's
    /// Turns count under its own Session's Workspace, not its parent's.
    Workspace,
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
    /// A Provider, every Model reached through it together.
    Provider { provider: ProviderId },
    /// A Workspace, by its identity: what the Sessions grouped there ran.
    Workspace { workspace: WorkspaceId },
    /// Turns whose Agent is not known, which no Model or Provider key can
    /// hold. Every Session has a Workspace, so that breakdown has none.
    Unknown,
}

/// A breakdown key and the name a reader is shown for it: a Model's name, a
/// Provider's display name, or a Workspace's name — told apart by where it
/// is where another Workspace goes by the same name.
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
    /// The buckets taken together, as though they were one: what a reader
    /// shows for the whole span it asked about. Its Session counts count a
    /// Session once however many buckets it has work in, which adding up
    /// the buckets' counts cannot.
    pub across_buckets: UsageMetricsBucket,
    /// The Turns no bucket can place, having neither begun nor Settled at a
    /// recorded moment: Turns stored before Suru recorded Turn timing.
    pub untimed: UsageMetricsBucket,
    /// The buckets and the untimed Turns taken together, as though they were
    /// one: what a reader asking for all of time shows for the whole of it.
    /// A Session with both timed and untimed work counts once, which adding
    /// `across_buckets` and `untimed` cannot.
    pub across_buckets_and_untimed: UsageMetricsAggregate,
    /// The figures of the query's comparison interval taken together, where
    /// it asked for one.
    pub comparison: Option<UsageMetricsAggregate>,
    /// The moment the earliest timed Turn on the Server is placed at, from
    /// which a reader asking for all of time can begin its buckets; absent
    /// where no Turn is timed.
    pub earliest_turn_at: Option<SessionTimestamp>,
    /// Every key a bucket breaks its figures down by, each with its label.
    pub keys: Vec<UsageMetricsKeyLabel>,
    /// The top-level Sessions whose work cost the most across the buckets,
    /// most first, as many as the query asked for at most: only those whose
    /// work there has a known Cost.
    pub top_sessions: Vec<UsageMetricsSession>,
}

/// One of the top-level Sessions whose work cost the most across a report's
/// buckets; a Sidekick's Subsession can be one. Its Cost is what its own Turns
/// and those of the Subagents beneath it contribute there, Cost Coverage
/// overlaps removed (ADR 0054), however much it cost outside them; a
/// Subsession's is its own, apart from the Sidekick whose tree Cost rolls it
/// up, since it is a Session of its own to open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsSession {
    pub session_id: SessionId,
    pub title: String,
    /// The name of the Session's Workspace, told apart from another's of the
    /// same name as a Workspace key's label is.
    pub workspace: String,
    /// The name of the Model the Session's work across the buckets cost the
    /// most under, or used the most tokens of where none cost more than
    /// another; absent where no Turn there knows its Agent.
    pub model: Option<String>,
    pub cost: Cost,
    /// Whether work of the Session across the buckets has a Cost no counted
    /// amount accounts for, so `cost` says less than it cost.
    pub is_cost_partial: bool,
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
/// keeps it, what their Cost Coverage contributions add up to, how many
/// Sessions they are the work of, and how many Turns they are and how those
/// ended.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMetricsAggregate {
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// The Cost the Turns contribute once every overlap is removed (ADR
    /// 0054), a running total's increments placed on the Turns that carried
    /// them; absent where none contributes a known amount, never a zero
    /// nobody measured.
    pub cost: Option<Cost>,
    /// Whether work among the Turns has a Cost no counted amount accounts
    /// for, so `cost` says less than they cost. It holds where `cost` is
    /// absent too: unpriced work is still work.
    pub is_cost_partial: bool,
    /// The part of `cost` Suru estimated rather than the Provider reported;
    /// absent where no contribution was Estimated.
    pub estimated_cost: Option<Cost>,
    /// How many distinct top-level Sessions — those without a parent, a
    /// Sidekick's Subsessions among them — ran at least one of the Turns.
    /// Distinct within the aggregate alone: a key's count and the bucket's
    /// total each count a Session once, so a Session that worked under two
    /// keys counts in both and once in the total.
    pub sessions: u64,
    /// How many distinct Subagent Sessions ran at least one of the Turns,
    /// counted apart from `sessions` so a delegating conversation is still
    /// one Session.
    pub subagent_sessions: u64,
    /// How many Turns there are among them, Settled or still working, each
    /// counted once however it ended.
    pub turns: u64,
    /// How many of those Turns Failed. A Turn a stopped process left working
    /// in a history still deferred reads as Active until its Session is
    /// hydrated, which settles it as Failed (ADR 0029), so opening a Session
    /// can raise this count without any new work.
    pub failed_turns: u64,
    /// How many of those Turns were Interrupted.
    pub interrupted_turns: u64,
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

/// What a reader asks of a Server's Insights: how the Sessions living there
/// worked over one span of time, rather than how much they consumed in each
/// of many. The Server places each Turn in it as it places one in a bucket.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InsightsQuery {
    /// Refused where it ends before it begins.
    pub interval: UsageMetricsInterval,
}

/// A Server's answer to an [`InsightsQuery`], over the Turns its interval
/// holds: those with a recorded moment to place them by, of the Sessions a
/// [`UsageMetricsReport`] counts. Each part is one panel's figures.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InsightsReport {
    pub turns: TurnInsights,
    /// The Turns placed in each UTC quarter-hour, earliest first: only those
    /// quarter-hours a Turn is placed in, so a long span stays small. Every
    /// UTC offset in use is a whole number of quarter-hours, so a reader
    /// folds these into its own calendar's hours exactly, daylight saving
    /// included, while the Server knows nothing of calendars.
    ///
    /// It grows with how many quarter-hours held work, never with how many
    /// Turns: at most 96 a day, so 35,040 for a year of work in every
    /// quarter-hour, each about 95 bytes as JSON — some 3.3 MB at most. All
    /// time grows by at most that much for each year worked, and in practice
    /// by the far fewer quarter-hours a reader works in.
    pub quarter_hours: Vec<QuarterHourTurns>,
    /// Every Tool the interval's Turns used, the most used first, those
    /// used as often the most failed first. Each use is an Activity of a
    /// Turn the interval holds, read from storage, so a Session working
    /// now may have used a Tool more recently than this says.
    pub tools: Vec<ToolTally>,
}

/// A Tool an Agent used, as the Insights tab tells them apart.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InsightsTool {
    /// A Command run in a shell.
    Command,
    /// A File Change made to the Workspace.
    FileChange,
    /// A delegation to a Subagent — spawn or resume — natively or through
    /// the Broker: each stretch of a Subagent's work stands as a Subagent
    /// row of its own.
    Subagent,
    /// Any other Tool, by its name as its Provider spells it, and the MCP
    /// server hosting it where it has one.
    Named {
        name: String,
        server: Option<String>,
    },
}

/// How often the interval's Turns used one Tool, and how many of those
/// uses failed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolTally {
    pub tool: InsightsTool,
    pub uses: u64,
    pub failed: u64,
}

/// How long a quarter-hour of [`InsightsReport::quarter_hours`] lasts, in
/// milliseconds.
pub const QUARTER_HOUR_MS: u64 = 15 * 60 * 1_000;

/// The Turns placed in one UTC quarter-hour — by when each began, otherwise
/// when it Settled, as the interval places them — and what they consumed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuarterHourTurns {
    /// When the quarter-hour begins: a whole number of [`QUARTER_HOUR_MS`]
    /// since the epoch.
    pub start: SessionTimestamp,
    pub turns: u64,
    /// Their blended tokens, on the same terms as every other surface's;
    /// absent where none of them reported any.
    pub tokens: Option<u64>,
    /// The Cost their Cost Coverage contributions add up to, overlaps
    /// removed as a [`UsageMetricsAggregate`]'s are, so the quarter-hours of
    /// an interval add up to its Cost; absent where none contributes a
    /// known amount.
    pub cost: Option<Cost>,
    /// Whether work among them has a Cost no counted amount accounts for.
    pub is_cost_partial: bool,
}

/// How long the interval's Turns took to Settle, and how they ended.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnInsights {
    /// Over every Settled Turn that recorded when it began and when it
    /// Settled; absent where none did. A Turn still working has no duration
    /// yet, and one missing either moment has none to give.
    pub durations: Option<TurnDurations>,
    pub completed: u64,
    pub failed: u64,
    pub interrupted: u64,
}

/// How many fixed bins [`TurnDurations::bins`] counts Turns in.
pub const TURN_DURATION_BINS: usize = 8;

/// Where each of [`TurnDurations::bins`] but the last ends, in milliseconds:
/// under 10s, 10–30s, 30s–1m, 1–2m, 2–5m, 5–15m, 15–30m, and the last bin
/// holding 30m and over. A bin holds a duration from where the one before it
/// ends up to but not including where it ends.
pub const TURN_DURATION_BIN_ENDS_MS: [u64; TURN_DURATION_BINS - 1] =
    [10_000, 30_000, 60_000, 120_000, 300_000, 900_000, 1_800_000];

/// The durations of a set of Turns, each its settle time less its start
/// time, in milliseconds. The median and p90 are nearest-rank: the duration
/// at least half, or nine in ten, of the Turns took no longer than.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnDurations {
    pub median_ms: u64,
    pub p90_ms: u64,
    pub longest_ms: u64,
    /// How many of the Turns took a duration in each bin, as
    /// [`TURN_DURATION_BIN_ENDS_MS`] cuts them.
    pub bins: [u64; TURN_DURATION_BINS],
}
