//! The Usage Metrics chart: a report reduced to the series it stacks, and
//! those series drawn as vertical bars over a y-axis of "nice" ticks and an
//! x-axis of local dates thinned so they never collide.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{UsageMetricsAggregate, UsageMetricsKey, UsageMetricsReport},
    theme::Theme,
};

use super::{
    super::usage::compact_count,
    calendar::{BucketUnit, CalendarBucket},
};

/// What the y-axis measures.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MetricsMeasure {
    /// The blended token figure every other surface shows.
    #[default]
    Tokens,
}

impl MetricsMeasure {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Tokens",
        }
    }

    fn value(self, aggregate: &UsageMetricsAggregate) -> u64 {
        match self {
            Self::Tokens => aggregate.blended_tokens().unwrap_or(0),
        }
    }

    /// A figure in the measure's unit, as compactly as every other surface
    /// writes it.
    pub(crate) fn format(self, value: u64) -> String {
        match self {
            Self::Tokens => compact_count(value),
        }
    }

    /// A range's whole, named in the measure's unit.
    pub(crate) fn total(self, value: u64) -> String {
        match self {
            Self::Tokens => format!("{} tokens", compact_count(value)),
        }
    }
}

/// One key's part of every bucket, in the measure the chart plots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Series {
    pub(crate) key: UsageMetricsKey,
    pub(crate) label: String,
    pub(crate) color: Color,
    /// The series' figure in each bucket, in the order of the buckets.
    pub(crate) values: Vec<u64>,
    pub(crate) total: u64,
}

/// The series a report stacks, bottom first: the largest range total at the
/// bottom and the rest above it in falling order, ties settled by label so
/// the order never shifts between reads of the same figures. Each takes the
/// colour of its place from the Theme's series palette.
pub(crate) fn series(
    report: &UsageMetricsReport,
    measure: MetricsMeasure,
    theme: &Theme,
) -> Vec<Series> {
    let mut series: Vec<Series> = report
        .keys
        .iter()
        .map(|label| {
            let values: Vec<u64> = report
                .buckets
                .iter()
                .map(|bucket| {
                    bucket
                        .by_key
                        .iter()
                        .find(|keyed| keyed.key == label.key)
                        .map_or(0, |keyed| measure.value(&keyed.aggregate))
                })
                .collect();
            Series {
                key: label.key.clone(),
                label: label.label.clone(),
                color: Color::Reset,
                total: values.iter().sum(),
                values,
            }
        })
        .collect();
    series.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.label.cmp(&b.label)));
    let palette = palette(theme);
    for (place, series) in series.iter_mut().enumerate() {
        series.color = palette[place % palette.len()];
    }
    series
}

/// The colours series take in turn: the Theme's accent, then the bright
/// ANSI hues it names, each once and none the canvas is painted with.
fn palette(theme: &Theme) -> Vec<Color> {
    let bright = theme.ansi.bright;
    let canvas = theme.surface.base.bg;
    let mut palette: Vec<Color> = Vec::new();
    for color in [
        theme.accent.primary.fg,
        Some(bright.blue),
        Some(bright.green),
        Some(bright.yellow),
        Some(bright.magenta),
        Some(bright.cyan),
        Some(bright.red),
    ]
    .into_iter()
    .flatten()
    {
        if color != Color::Reset && Some(color) != canvas && !palette.contains(&color) {
            palette.push(color);
        }
    }
    if palette.is_empty() {
        palette.push(Color::Reset);
    }
    palette
}

/// A y-axis: its top, and the step between its ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Axis {
    pub(crate) top: u64,
    pub(crate) step: u64,
}

impl Axis {
    pub(crate) fn ticks(self) -> u64 {
        self.top / self.step
    }
}

/// The axis reaching `max` in at most `ticks` steps of one, two, or five
/// times a power of ten, with the smallest step that does.
pub(crate) fn nice_axis(max: u64, ticks: u64) -> Axis {
    let max = u128::from(max.max(1));
    let ticks = u128::from(ticks.max(1));
    let mut magnitude: u128 = 1;
    loop {
        for multiple in [1, 2, 5] {
            let step = multiple * magnitude;
            if step * ticks >= max {
                let top = max.div_ceil(step) * step;
                return Axis {
                    top: u64::try_from(top).unwrap_or(u64::MAX),
                    step: u64::try_from(step).unwrap_or(u64::MAX),
                };
            }
        }
        magnitude *= 10;
    }
}

/// How a bucket is named under its bar.
pub(crate) fn bucket_label(bucket: &CalendarBucket, unit: BucketUnit) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match unit {
        BucketUnit::Day => {
            let date = bucket.begins.date();
            let month = MONTHS[usize::try_from(date.month() - 1).unwrap_or(0)];
            format!("{} {month}", date.day())
        }
    }
}

/// The labels shown along an x-axis whose bars stand `slot` columns apart,
/// each as its bucket's index and the column it begins at, counted from the
/// first bar. Every `stride`-th bucket is labelled, counting back from the
/// last so the present is always named, the stride the shortest of `strides`
/// — the spans a reader counts the unit in, such as a week of days — that
/// leaves a column between labels. The last label moves left as far as it must to
/// end within `width`; any other stands under its own bar or not at all, so
/// one that would still run into its right-hand neighbour is dropped.
pub(crate) fn thinned_labels(
    labels: &[String],
    strides: &[u16],
    slot: u16,
    width: u16,
) -> Vec<(usize, u16)> {
    let widest = labels.iter().map(|label| label.width()).max().unwrap_or(0);
    let needed = u16::try_from(widest + 1).unwrap_or(u16::MAX);
    let slot = slot.max(1);
    let stride = strides
        .iter()
        .copied()
        .find(|stride| stride * slot >= needed)
        .unwrap_or_else(|| needed.div_ceil(slot));
    let mut placed: Vec<(usize, u16)> = Vec::new();
    for index in (0..labels.len()).rev().step_by(usize::from(stride)) {
        let label_width = u16::try_from(labels[index].width()).unwrap_or(u16::MAX);
        let under_bar = u16::try_from(index)
            .unwrap_or(u16::MAX)
            .saturating_mul(slot);
        let at = match placed.last() {
            None if label_width <= width => under_bar.min(width - label_width),
            Some(&(_, neighbour)) if under_bar + label_width < neighbour => under_bar,
            _ => continue,
        };
        placed.push((index, at));
    }
    placed.reverse();
    placed
}

/// Eighths of a cell filled from its foot.
const LOWER_EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Draws `series` stacked over `buckets` into `area`: the y-axis and its tick
/// labels down the left, a bar per bucket, the x-axis beneath them, and the
/// thinned bucket labels under that. Each segment boundary falling inside a
/// cell is drawn with an eighth block in the lower segment's colour over the
/// upper segment's, so a segment's height reads to an eighth of a row.
pub(crate) fn render_chart(
    buffer: &mut Buffer,
    area: Rect,
    buckets: &[CalendarBucket],
    unit: BucketUnit,
    series: &[Series],
    measure: MetricsMeasure,
    theme: &Theme,
) {
    // The x-axis and the labels beneath it take the last two rows.
    let plot_rows = area.height.saturating_sub(2);
    if plot_rows == 0 || buckets.is_empty() {
        return;
    }
    let totals: Vec<u64> = (0..buckets.len())
        .map(|index| series.iter().map(|series| series.values[index]).sum())
        .collect();
    let max = totals.iter().copied().max().unwrap_or(0);
    // A tick every other row at most, so the axis never reads as a ladder.
    let axis = nice_axis(max, u64::from(plot_rows / 2).max(1));
    let ticks = u16::try_from(axis.ticks()).unwrap_or(u16::MAX).max(1);
    let rows_per_tick = (plot_rows / ticks).max(1);
    let plot_rows = rows_per_tick.saturating_mul(ticks).min(plot_rows);
    let tick_labels: Vec<String> = (1..=u64::from(ticks))
        .map(|tick| measure.format(tick * axis.step))
        .collect();
    let label_width = u16::try_from(
        tick_labels
            .iter()
            .map(|label| label.width())
            .max()
            .unwrap_or(0),
    )
    .unwrap_or(0);
    let axis_x = area.x + label_width + 1;
    let plot_x = axis_x + 1;
    let plot_width = area.right().saturating_sub(plot_x);
    let top = area.y + (area.height - 2 - plot_rows);
    let axis_y = top + plot_rows;
    let subdued = theme.text.subdued;
    let border = theme.border.subdued;

    for row in 0..plot_rows {
        let y = top + row;
        let from_foot = plot_rows - row;
        let tick = from_foot % rows_per_tick == 0;
        if tick {
            let label = &tick_labels[usize::from(from_foot / rows_per_tick) - 1];
            let x = axis_x - 1 - u16::try_from(label.width()).unwrap_or(0);
            buffer.set_string(x, y, label, subdued);
        }
        buffer.set_string(axis_x, y, if tick { "┤" } else { "│" }, border);
    }
    let count = u16::try_from(buckets.len()).unwrap_or(u16::MAX);
    let slot = (plot_width / count).max(1);
    // The axis runs as far as the bars do, and no further.
    buffer.set_string(axis_x, axis_y, "└", border);
    let axis_end = plot_x
        .saturating_add(slot.saturating_mul(count))
        .saturating_add(1)
        .min(area.right());
    for x in plot_x..axis_end {
        buffer.set_string(x, axis_y, "─", border);
    }
    let bar_width = if slot >= 2 { slot - 1 } else { 1 };
    let eighths = u128::from(plot_rows) * 8;
    for (index, _) in buckets.iter().enumerate() {
        let bar_x = plot_x
            + u16::try_from(index)
                .unwrap_or(u16::MAX)
                .saturating_mul(slot);
        let bar_x = bar_x + (slot - bar_width);
        if bar_x + bar_width > area.right() {
            break;
        }
        // Where each segment ends, in eighths of a row from the foot.
        let mut boundaries = Vec::with_capacity(series.len());
        let mut running: u128 = 0;
        for series in series {
            running += u128::from(series.values[index]);
            let ends = (running * eighths + u128::from(axis.top) / 2) / u128::from(axis.top);
            boundaries.push((u32::try_from(ends).unwrap_or(u32::MAX), series.color));
        }
        for row in 0..plot_rows {
            let foot = u32::from(row) * 8;
            let Some(lower) = segment_at(&boundaries, foot) else {
                break;
            };
            let filled = boundaries[lower].0.min(foot + 8) - foot;
            let mut style = Style::default().fg(boundaries[lower].1);
            if filled < 8
                && let Some(upper) = segment_at(&boundaries, foot + 7)
            {
                style = style.bg(boundaries[upper].1);
            }
            let glyph = LOWER_EIGHTHS[usize::try_from(filled).unwrap_or(8)].to_string();
            let y = axis_y - 1 - row;
            for x in bar_x..bar_x + bar_width {
                buffer.set_string(x, y, &glyph, style);
            }
        }
    }

    let labels: Vec<String> = buckets
        .iter()
        .map(|bucket| bucket_label(bucket, unit))
        .collect();
    let first_bar = slot - bar_width;
    for (index, at) in thinned_labels(
        &labels,
        unit.label_strides(),
        slot,
        plot_width.saturating_sub(first_bar),
    ) {
        buffer.set_string(plot_x + first_bar + at, axis_y + 1, &labels[index], subdued);
    }
}

/// The segment holding eighth `eighth` from the foot, if any reaches it.
fn segment_at(boundaries: &[(u32, Color)], eighth: u32) -> Option<usize> {
    let mut floor = 0;
    for (index, (ends, _)) in boundaries.iter().enumerate() {
        if floor <= eighth && eighth < *ends {
            return Some(index);
        }
        floor = floor.max(*ends);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_axis_steps_by_one_two_or_five_times_a_power_of_ten() {
        assert_eq!(nice_axis(184, 4), Axis { top: 200, step: 50 });
        assert_eq!(
            nice_axis(36_500_000, 4),
            Axis {
                top: 40_000_000,
                step: 10_000_000
            }
        );
        assert_eq!(nice_axis(7, 8), Axis { top: 7, step: 1 });
        assert_eq!(nice_axis(0, 4), Axis { top: 1, step: 1 });
        assert_eq!(
            nice_axis(1_000, 1),
            Axis {
                top: 1_000,
                step: 1_000
            }
        );
        assert_eq!(
            nice_axis(1_001, 1),
            Axis {
                top: 2_000,
                step: 2_000
            }
        );
    }

    #[test]
    fn labels_are_thinned_so_none_runs_into_the_next() {
        let labels: Vec<String> = (1..=30).map(|day| format!("{day} Oct")).collect();

        let placed = thinned_labels(&labels, BucketUnit::Day.label_strides(), 3, 90);

        assert_eq!(
            placed.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![1, 8, 15, 22, 29],
            "a week apart, counting back from today"
        );
        for pair in placed.windows(2) {
            let (left, at) = pair[0];
            assert!(at + (labels[left].width() as u16) < pair[1].1);
        }
        let (last, at) = *placed.last().unwrap();
        assert!(at + labels[last].width() as u16 <= 90);
    }

    #[test]
    fn a_label_at_the_right_edge_moves_left_rather_than_running_off() {
        let labels: Vec<String> = ["1 Oct", "2 Oct", "3 Oct"].map(str::to_owned).into();

        let placed = thinned_labels(&labels, BucketUnit::Day.label_strides(), 6, 15);

        assert_eq!(placed, vec![(0, 0), (2, 10)]);
    }
}
