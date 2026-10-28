//! The Usage Metrics headline: the range's figures taken together — Cost,
//! Tokens, Sessions, Turns, and the share of input served from cache — each
//! compared with the period before it, and the Usage mix strip the Tokens
//! measure shows under its legend.

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::UsageMetricsAggregate, theme::Theme};

use super::{
    super::{
        slots::truncate_to_width,
        usage::{compact_cost, compact_count},
    },
    calendar::MetricsRange,
    chart::MetricsMeasure,
};

/// The rows the headline takes: its figures, their comparisons, and what
/// each says of itself besides.
pub(super) const HEADLINE_ROWS: u16 = 3;
/// The rows the Usage mix strip takes: its bar, and the key beneath it.
pub(super) const MIX_ROWS: u16 = 2;
/// The widest the columns of the headline stand apart.
const WIDEST_GAP: usize = 8;
/// The narrowest they stand apart before their words are shortened.
const NARROWEST_GAP: usize = 2;
/// What stands before the Usage mix strip's bar.
const MIX_LEAD: &str = "Usage mix ";
/// The Usage mix strip's bar runs no longer than this.
const MIX_BAR: usize = 64;

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
struct Column {
    figure: String,
    compared: Option<String>,
    detail: Option<(String, String)>,
}

impl Column {
    fn width(&self, short: bool) -> usize {
        let detail = self
            .detail
            .as_ref()
            .map(|(long, brief)| if short { brief } else { long });
        [Some(&self.figure), self.compared.as_ref(), detail]
            .into_iter()
            .flatten()
            .map(|line| line.width())
            .max()
            .unwrap_or(0)
    }
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
    let gaps = unnamed.len().saturating_sub(1).max(1);
    let spare = |columns: &[Column], short: bool| {
        let used: usize = columns.iter().map(|column| column.width(short)).sum();
        room.saturating_sub(used) / gaps
    };
    // The period is named after the first comparison where there is room
    // for it; then the longer words are kept where they leave room, and
    // the shorter taken where even those leave the columns crowded.
    let mut named = columns(whole, before);
    if let (Some(label), Some(first)) = (
        range.compared_with(),
        named.iter_mut().find_map(|column| column.compared.as_mut()),
    ) {
        first.push(' ');
        first.push_str(label);
    }
    let (columns, short) = if spare(&named, false) >= NARROWEST_GAP {
        (named, false)
    } else {
        let short = spare(&unnamed, false) < NARROWEST_GAP;
        (unnamed, short)
    };
    let gap = spare(&columns, short).clamp(1, WIDEST_GAP);

    let compared = before.is_some();
    let mut x = usize::from(area.x);
    for column in &columns {
        let width = column.width(short);
        let left = room.saturating_sub(x - usize::from(area.x));
        if left == 0 {
            break;
        }
        let detail = column
            .detail
            .as_ref()
            .map(|(long, brief)| if short { brief } else { long });
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
            vec![lines[0], lines[1], lines[2]]
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
        x += width + gap;
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
    if let Some(share) = cache_share(whole) {
        columns.push(Column {
            figure: format!("{share}% from cache"),
            compared: before
                .and_then(cache_share)
                .map(|earlier| arrowed(i64::from(share) - i64::from(earlier), " pts")),
            detail: None,
        });
    }
    columns
}

/// `count` of a thing, named in the singular or the plural.
fn counted(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", compact_count(count))
    }
}

/// The share of `aggregate`'s input its cache reads served, as a whole
/// percentage: cache reads ÷ (fresh input + cache reads + cache writes),
/// where any input was recorded at all.
fn cache_share(aggregate: &UsageMetricsAggregate) -> Option<u8> {
    let reads = aggregate.cache_read_tokens.unwrap_or(0);
    let input = [
        aggregate.fresh_input_tokens,
        aggregate.cache_read_tokens,
        aggregate.cache_write_tokens,
    ]
    .into_iter()
    .flatten()
    .map(u128::from)
    .sum::<u128>();
    if input == 0 {
        return None;
    }
    let percent = (u128::from(reads) * 100 + input / 2) / input;
    u8::try_from(percent).ok()
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

/// The parts the Usage mix strip divides its bar among, in its order, each
/// with the shade it is drawn in and its names in the key, long and short.
const MIX_PARTS: [(char, &str, &str); 4] = [
    ('█', "fresh input", "input"),
    ('▓', "cache reads", "cache read"),
    ('▒', "cache writes", "cache write"),
    ('░', "output", "output"),
];

/// The parts of `aggregate`'s Usage the strip shows, in [`MIX_PARTS`]
/// order: each one reported, however small, and none that was not.
fn mix_parts(aggregate: &UsageMetricsAggregate) -> [Option<u64>; 4] {
    [
        aggregate.fresh_input_tokens,
        aggregate.cache_read_tokens,
        aggregate.cache_write_tokens,
        aggregate.output_tokens,
    ]
}

/// Draws the Usage mix strip in the first [`MIX_ROWS`] of `area`: one bar
/// divided among fresh input, cache reads, cache writes, and output, each
/// as its share of them all, and a key beneath naming each share and how
/// much of the output was reasoning. A part never reported has no place in
/// either; where nothing was reported there is no strip.
pub(super) fn render_mix(
    buffer: &mut Buffer,
    area: Rect,
    aggregate: &UsageMetricsAggregate,
    theme: &Theme,
) {
    let parts = mix_parts(aggregate);
    let whole: u64 = parts
        .iter()
        .flatten()
        .fold(0, |sum, part| sum.saturating_add(*part));
    if whole == 0 || area.height == 0 || area.is_empty() {
        return;
    }
    let lead_width = u16::try_from(MIX_LEAD.width()).unwrap_or(u16::MAX);
    buffer.set_string(area.x, area.y, MIX_LEAD, theme.text.subdued);
    let bar_width = usize::from(area.width.saturating_sub(lead_width)).min(MIX_BAR);
    let shade = theme.accent.primary;
    let cells = cells(&parts, whole, bar_width);
    let bar: String = MIX_PARTS
        .iter()
        .zip(&cells)
        .map(|((glyph, _, _), cells)| glyph.to_string().repeat(*cells))
        .collect();
    buffer.set_string(area.x + lead_width, area.y, bar, shade);
    if area.height < 2 {
        return;
    }
    let key = |short: bool| {
        let mut entries = Vec::new();
        for ((glyph, long, brief), part) in MIX_PARTS.iter().zip(parts) {
            let Some(part) = part else { continue };
            let name = if short { brief } else { long };
            let mut entry = format!("{glyph} {name} {}", share(part, whole));
            if *glyph == '░'
                && let Some(reasoning) = aggregate.reasoning_tokens
                && part > 0
            {
                let reasoning = share(reasoning.min(part), part);
                if short {
                    entry.push_str(&format!(" ({reasoning} reasoning)"));
                } else {
                    entry.push_str(&format!(", {reasoning} of it reasoning"));
                }
            }
            entries.push(entry);
        }
        entries.join("  ")
    };
    let room = usize::from(area.width);
    let long = key(false);
    let key = if long.width() <= room {
        long
    } else {
        key(true)
    };
    let key = truncate_to_width(&key, room);
    let y = area.y + 1;
    buffer.set_string(area.x, y, &key, theme.text.subdued);
    // The swatches take the bar's own shade.
    let mut x = area.x;
    for character in key.chars() {
        if MIX_PARTS.iter().any(|(glyph, _, _)| *glyph == character) {
            buffer.set_string(x, y, character.to_string(), shade);
        }
        x += u16::try_from(character.to_string().width()).unwrap_or(1);
    }
}

/// How many of `width` cells each of `parts` takes, in proportion to its
/// share of `whole`, by the largest remainder, so they fill the bar exactly
/// and every part with any tokens takes a cell where there are cells enough.
fn cells(parts: &[Option<u64>; 4], whole: u64, width: usize) -> [usize; 4] {
    let whole = u128::from(whole.max(1));
    let width_ = width as u128;
    let exact = parts.map(|part| u128::from(part.unwrap_or(0)) * width_);
    let mut cells = exact.map(|exact| usize::try_from(exact / whole).unwrap_or(0));
    let mut left = width.saturating_sub(cells.iter().sum());
    let mut by_remainder: Vec<usize> = (0..parts.len()).collect();
    by_remainder.sort_by_key(|index| std::cmp::Reverse(exact[*index] % whole));
    for index in by_remainder {
        if left == 0 {
            break;
        }
        if exact[index] > 0 {
            cells[index] += 1;
            left -= 1;
        }
    }
    // A sliver still shows, taking its cell from the largest part.
    for index in 0..parts.len() {
        if parts[index].is_some_and(|part| part > 0) && cells[index] == 0 {
            let largest = (0..parts.len())
                .max_by_key(|other| cells[*other])
                .expect("four parts");
            if cells[largest] > 1 {
                cells[largest] -= 1;
                cells[index] = 1;
            }
        }
    }
    cells
}

/// `part` of `whole` as a whole percentage, a sliver that rounds to nothing
/// still shown as there.
fn share(part: u64, whole: u64) -> String {
    let whole = u128::from(whole.max(1));
    let percent = (u128::from(part) * 100 + whole / 2) / whole;
    if percent == 0 && part > 0 {
        "<1%".to_owned()
    } else {
        format!("{percent}%")
    }
}

#[cfg(test)]
mod tests {
    use super::{cells, percent_change};

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
    fn the_mix_fills_its_bar_and_a_sliver_still_takes_a_cell() {
        assert_eq!(
            cells(&[Some(1), Some(1), None, Some(2)], 4, 8),
            [2, 2, 0, 4]
        );
        assert_eq!(
            cells(&[Some(1), Some(10_000), None, Some(0)], 10_001, 10),
            [1, 9, 0, 0]
        );
    }
}
