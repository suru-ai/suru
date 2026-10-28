//! The Usage Metrics headline: the range's figures taken together — Cost,
//! Tokens, Sessions, Turns, and the share of input served from cache — each
//! compared with the period before it.

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::UsageMetricsAggregate, theme::Theme};

use super::{
    super::{
        slots::truncate_to_width,
        usage::{compact_cost, compact_count},
    },
    calendar::MetricsRange,
    chart::{MetricsMeasure, counted},
};

/// The rows the headline takes: its figures, their comparisons, and what
/// each says of itself besides.
pub(super) const HEADLINE_ROWS: u16 = 3;
/// The widest the columns of the headline stand apart.
const WIDEST_GAP: usize = 8;
/// The narrowest they stand apart before their words are shortened.
const NARROWEST_GAP: usize = 2;

/// Whether `aggregate` holds anything a headline could state: Usage, a
/// Cost, work whose Cost is unknown, or Turns at all.
pub(super) fn holds_anything(aggregate: &UsageMetricsAggregate) -> bool {
    holds_usage(aggregate) || aggregate.is_cost_partial || aggregate.turns > 0
}

/// Whether `aggregate` holds any Usage or Cost: a period that held none is
/// no period to compare with.
pub(super) fn holds_usage(aggregate: &UsageMetricsAggregate) -> bool {
    let parts = [
        aggregate.fresh_input_tokens,
        aggregate.cache_read_tokens,
        aggregate.cache_write_tokens,
        aggregate.output_tokens,
        aggregate.reasoning_tokens,
    ];
    parts.into_iter().flatten().any(|part| part > 0) || aggregate.cost.is_some()
}

/// One column of the headline: a figure, how it compares with the period
/// before, and what it says of itself besides, in a longer and a shorter
/// form for a narrow frame.
#[derive(Clone)]
struct Column {
    figure: String,
    compared: Option<String>,
    detail: Option<(String, String)>,
}

impl Column {
    /// The columns its figure and comparison take: what it says beneath
    /// them may run on under the columns after it.
    fn width(&self) -> usize {
        [Some(&self.figure), self.compared.as_ref()]
            .into_iter()
            .flatten()
            .map(|line| line.width())
            .max()
            .unwrap_or(0)
    }

    fn detail(&self, short: bool) -> Option<&String> {
        self.detail
            .as_ref()
            .map(|(long, brief)| if short { brief } else { long })
    }
}

/// Where each of `columns` begins, counted from the headline's left edge,
/// each `gap` columns after the one before it ends, and the columns they all
/// take. A column is as wide as its figure and its comparison; what it says
/// beneath them runs on under the columns after it that say nothing there,
/// so a long word or two pushes nothing aside, and widens it only as far as
/// that leaves a gap before the next column to say something beneath.
fn place(columns: &[Column], short: bool, gap: usize) -> (Vec<usize>, usize) {
    let details: Vec<Option<usize>> = columns
        .iter()
        .map(|column| column.detail(short).map(|detail| detail.width()))
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(Column::width).collect();
    for at in 0..columns.len() {
        let Some(detail) = details[at] else { continue };
        let Some(next) = (at + 1..columns.len()).find(|next| details[*next].is_some()) else {
            continue;
        };
        let beneath: usize = widths[at + 1..next].iter().map(|width| width + gap).sum();
        widths[at] = widths[at].max(detail.saturating_sub(beneath));
    }
    let mut starts = Vec::with_capacity(columns.len());
    let (mut x, mut end) = (0, 0);
    for (width, detail) in widths.iter().zip(&details) {
        starts.push(x);
        end = end.max(x + width).max(x + detail.unwrap_or(0));
        x += width + gap;
    }
    (starts, end)
}

/// Draws the headline in the first [`HEADLINE_ROWS`] of `area`: `whole`'s
/// figures, each compared with `before`'s where it holds Usage and its own
/// figure is not nothing, the first of those named for `range`'s period.
/// Where nothing is compared, what each figure says besides closes up
/// under it.
pub(super) fn render_headline(
    buffer: &mut Buffer,
    area: Rect,
    whole: &UsageMetricsAggregate,
    before: Option<&UsageMetricsAggregate>,
    range: MetricsRange,
    theme: &Theme,
) {
    let before = before.filter(|before| holds_usage(before));
    let unnamed = columns(whole, before);
    let room = usize::from(area.width);
    // The widest gap, up to the widest allowed, that leaves the columns
    // within the headline's room, where even the narrowest allowed does.
    let widest_gap = |columns: &[Column], short: bool| {
        (NARROWEST_GAP..=WIDEST_GAP)
            .rev()
            .find(|gap| place(columns, short, *gap).1 <= room)
    };
    // The period is named after the first comparison where there is room
    // for it; then the longer words are kept where they leave room, and
    // the shorter taken where even those leave the columns crowded.
    let mut named = unnamed.clone();
    if let (Some(label), Some(first)) = (
        range.compared_with(),
        named.iter_mut().find_map(|column| column.compared.as_mut()),
    ) {
        first.push(' ');
        first.push_str(&label);
    }
    let (columns, short, gap) = if let Some(gap) = widest_gap(&named, false) {
        (named, false, gap)
    } else if let Some(gap) = widest_gap(&unnamed, false) {
        (unnamed, false, gap)
    } else {
        let gap = widest_gap(&unnamed, true).unwrap_or(1);
        (unnamed, true, gap)
    };
    let (starts, _) = place(&columns, short, gap);

    let compared = before.is_some();
    for (column, start) in columns.iter().zip(starts) {
        let left = room.saturating_sub(start);
        if left == 0 {
            break;
        }
        let x = usize::from(area.x) + start;
        let detail = column.detail(short);
        let lines = [
            Some((
                &column.figure,
                theme.text.primary.add_modifier(Modifier::BOLD),
            )),
            column
                .compared
                .as_ref()
                .map(|compared| (compared, theme.text.subdued)),
            detail.map(|detail| (detail, theme.text.subdued)),
        ];
        // Each comparison keeps its row, so the details stand level, unless
        // nothing is compared at all.
        let rows = if compared {
            lines.to_vec()
        } else {
            vec![lines[0], lines[2]]
        };
        for (row, line) in rows.into_iter().enumerate() {
            let Some((text, style)) = line else { continue };
            let Some(y) = u16::try_from(row)
                .ok()
                .map(|row| area.y + row)
                .filter(|y| *y < area.bottom())
            else {
                break;
            };
            let at = u16::try_from(x).unwrap_or(u16::MAX);
            buffer.set_string(at, y, truncate_to_width(text, left), style);
        }
    }
}

/// The headline's columns for `whole`, compared with `before` where given.
fn columns(whole: &UsageMetricsAggregate, before: Option<&UsageMetricsAggregate>) -> Vec<Column> {
    let change = |figure: fn(&UsageMetricsAggregate) -> u64| {
        before.and_then(|before| percent_change(figure(whole), figure(before)))
    };
    let cost = |aggregate: &UsageMetricsAggregate| MetricsMeasure::Cost.value(aggregate);
    let tokens = |aggregate: &UsageMetricsAggregate| MetricsMeasure::Tokens.value(aggregate);
    let sessions = |aggregate: &UsageMetricsAggregate| aggregate.sessions;
    let turns = |aggregate: &UsageMetricsAggregate| aggregate.turns;

    let estimated = whole.estimated_cost.map(|estimated| {
        let estimated = compact_cost(estimated);
        (
            format!("{estimated} estimated"),
            format!("{estimated} est."),
        )
    });
    let subagents = (whole.subagent_sessions > 0).then(|| {
        let subagents = counted(whole.subagent_sessions, "Subagent", "Subagents");
        (subagents.clone(), subagents)
    });
    let ended = |interrupted: &str| {
        let mut parts = Vec::new();
        if whole.failed_turns > 0 {
            parts.push(format!("{} failed", compact_count(whole.failed_turns)));
        }
        if whole.interrupted_turns > 0 {
            parts.push(format!(
                "{} {interrupted}",
                compact_count(whole.interrupted_turns)
            ));
        }
        parts.join(" · ")
    };
    let ended = (whole.failed_turns > 0 || whole.interrupted_turns > 0)
        .then(|| (ended("interrupted"), ended("intr.")));

    let mut columns = vec![
        Column {
            figure: MetricsMeasure::Cost.total(cost(whole), whole.is_cost_partial),
            compared: change(cost),
            detail: estimated,
        },
        Column {
            figure: MetricsMeasure::Tokens.total(tokens(whole), false),
            compared: change(tokens),
            detail: None,
        },
        Column {
            figure: MetricsMeasure::Sessions.total(whole.sessions, false),
            compared: change(sessions),
            detail: subagents,
        },
        Column {
            figure: counted(whole.turns, "Turn", "Turns"),
            compared: change(turns),
            detail: ended,
        },
    ];
    if let Some(share) = whole.cache_read_share() {
        columns.push(Column {
            figure: format!("{}% from cache", (share + 50) / 100),
            compared: before
                .and_then(UsageMetricsAggregate::cache_read_share)
                .map(|earlier| points_change(share, earlier)),
            detail: None,
        });
    }
    columns
}

/// How far a share of `now` hundredths of a percent rose or fell from one
/// of `before`, in whole percentage points, rounded once from the shares
/// themselves rather than from the whole percentages each is shown as.
fn points_change(now: u16, before: u16) -> String {
    let change = i64::from(now) - i64::from(before);
    // Rounded half away from nought, as a percent change is.
    let points = (change * 2 + change.signum() * 100) / 200;
    arrowed(points, if points.abs() == 1 { " pt" } else { " pts" })
}

/// How far `now` rose or fell from `before`, as a whole percentage of it;
/// none where `before` was nothing, which no change is a share of.
fn percent_change(now: u64, before: u64) -> Option<String> {
    if before == 0 {
        return None;
    }
    let (now, before) = (i128::from(now), i128::from(before));
    let change = now - before;
    // Rounded half away from nought, so a rise and a fall alike read true.
    let percent = (change * 200 + change.signum() * before) / (before * 2);
    Some(arrowed(i64::try_from(percent).unwrap_or(i64::MAX), "%"))
}

/// A change written with the arrow of its direction.
fn arrowed(change: i64, unit: &str) -> String {
    match change.signum() {
        1 => format!("▲ {change}{unit}"),
        -1 => format!("▼ {}{unit}", change.unsigned_abs()),
        _ => format!("± 0{unit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{percent_change, points_change};

    #[test]
    fn a_change_is_a_whole_percentage_of_the_period_before_rounded_either_way() {
        assert_eq!(percent_change(118, 100).as_deref(), Some("▲ 18%"));
        assert_eq!(percent_change(90, 100).as_deref(), Some("▼ 10%"));
        assert_eq!(percent_change(1005, 1000).as_deref(), Some("▲ 1%"));
        assert_eq!(percent_change(995, 1000).as_deref(), Some("▼ 1%"));
        assert_eq!(percent_change(1004, 1000).as_deref(), Some("± 0%"));
        assert_eq!(percent_change(5, 0), None, "nothing before to compare");
    }

    #[test]
    fn a_share_s_change_is_rounded_once_from_the_shares_themselves() {
        // 71.2% and 71.0% each show as 71%: no point between them.
        assert_eq!(points_change(7_120, 7_100), "± 0 pts");
        // 71.4% and 70.6% both show as 71%, yet are most of a point apart.
        assert_eq!(points_change(7_140, 7_060), "▲ 1 pt");
        // 70.4% and 70.6% show as 70% and 71%, yet are not a point apart.
        assert_eq!(points_change(7_040, 7_060), "± 0 pts");
        assert_eq!(points_change(7_100, 6_000), "▲ 11 pts");
        assert_eq!(points_change(6_000, 7_150), "▼ 12 pts");
    }
}
