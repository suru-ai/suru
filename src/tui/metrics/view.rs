//! Draws the Usage Metrics view over the whole frame: a title bar with its
//! close control, the range bar and the measures beside it, then the range's
//! headline figures, the chart, and its legend — with the Usage mix strip
//! under it for Tokens — or what stands in for them while a read is on its
//! way, when it was refused, or when the range holds no Usage.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsReport},
    theme::Theme,
};

use super::{
    super::slots::truncate_to_width,
    BREAKDOWNS, FocusedRow, MetricsContent, MetricsView, OfferedBreakdown, UsageMetricsReadRefusal,
    calendar::{MetricsRange, RangeBuckets},
    chart::{self, MetricsMeasure, Series},
    headline::{self, HEADLINE_ROWS, MIX_ROWS},
};

/// The close control, pointable as a whole.
const CLOSE: &str = "esc ✕";
const REFRESH: &str = "r refresh";
/// Columns the view's content stands in from either edge.
const INSET: u16 = 2;
/// The legend's proportional bars run no longer than this.
const LEGEND_BAR: u16 = 24;
/// Labels in the legend are cut to this many columns.
const LEGEND_LABEL: usize = 32;
/// Columns between a row of tabs and what follows it on the controls row.
const CONTROL_GAP: u16 = 4;
/// Columns between one tab's place in a row of them and the next's.
const TAB_GAP: u16 = 1;
/// What stands before the breakdowns on the controls row.
const BREAKDOWN_LEAD: &str = "by ";
/// What stands in for the chart where All time's every Turn is untimed.
const UNCHARTED: &str =
    "This Usage was recorded before Suru timed Turns, so none of it can be charted.";

pub(in crate::tui) fn render(frame: &mut Frame<'_>, view: &MetricsView, theme: &Theme) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    view.forget_targets();
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;

    buffer.set_string(
        area.x + 1,
        area.y,
        "Metrics",
        primary.add_modifier(Modifier::BOLD),
    );
    let close_width = u16::try_from(CLOSE.width()).unwrap_or(0);
    let close_x = area.right().saturating_sub(close_width + 1);
    buffer.set_string(close_x, area.y, CLOSE, subdued);
    view.record_close_target(Rect::new(close_x, area.y, close_width, 1));
    let refresh_x = close_x.saturating_sub(u16::try_from(REFRESH.width()).unwrap_or(0) + 3);
    buffer.set_string(refresh_x, area.y, REFRESH, subdued);
    for x in area.x + 1..area.right().saturating_sub(1) {
        buffer.set_string(x, area.y + 1, "─", theme.border.subdued);
    }

    let range = view.range();
    let measure = view.measure();
    let controls_y = area.y + 3;
    let bar_end = render_tabs(
        buffer,
        area.x + INSET - 1,
        controls_y,
        MetricsRange::ALL.map(|chosen| (chosen.label(), chosen == range)),
        |place, at| view.record_range_target(MetricsRange::ALL[place], at),
        theme,
    );
    let measures_end = render_tabs(
        buffer,
        bar_end + CONTROL_GAP - 1,
        controls_y,
        MetricsMeasure::ALL.map(|chosen| (chosen.label(), chosen == measure)),
        |place, at| view.record_measure_target(MetricsMeasure::ALL[place], at),
        theme,
    );
    let breakdown = view.breakdown();
    let lead_x = measures_end + CONTROL_GAP;
    buffer.set_string(lead_x, controls_y, BREAKDOWN_LEAD, subdued);
    render_tabs(
        buffer,
        lead_x + u16::try_from(BREAKDOWN_LEAD.width()).unwrap_or(0),
        controls_y,
        BREAKDOWNS.map(|chosen| (chosen.label(), chosen == breakdown)),
        |place, at| view.record_breakdown_target(BREAKDOWNS[place], at),
        theme,
    );
    // What is happening to the figures shown takes the row under the
    // controls, where it has the whole width to say it in.
    let status_y = controls_y + 1;
    let room = usize::from(area.width.saturating_sub(INSET * 2));
    if let Some(refusal) = view.stale_because() {
        // Figures that could not be read again stay, saying why above them.
        let note = truncate_to_width(&stale_note(refusal), room);
        buffer.set_string(area.x + INSET, status_y, note, theme.feedback.error);
    } else if view.is_reading() && matches!(view.content(), MetricsContent::Report { .. }) {
        buffer.set_string(area.x + INSET, status_y, "Refreshing…", subdued);
    }

    let mut body = Rect::new(
        area.x + INSET,
        area.y + 5,
        area.width.saturating_sub(INSET * 2),
        area.height.saturating_sub(6),
    );
    // The headline states the range's figures above everything the report
    // draws, the hidden series among them: All time's count every Turn,
    // untimed ones too, where the chart can place only the timed.
    let mut headed = false;
    if let MetricsContent::Report { report, .. } = view.content() {
        let whole = if range == MetricsRange::AllTime {
            &report.across_buckets_and_untimed
        } else {
            &report.across_buckets.total
        };
        if headline::holds_anything(whole) && body.height > 0 {
            let rows = HEADLINE_ROWS.min(body.height);
            headline::render_headline(
                buffer,
                Rect::new(body.x, body.y, body.width, rows),
                whole,
                report.comparison.as_ref(),
                range,
                theme,
            );
            let taken = (HEADLINE_ROWS + 1).min(body.height);
            body = Rect::new(body.x, body.y + taken, body.width, body.height - taken);
            headed = true;
        }
    }
    if matches!(view.content(), MetricsContent::Report { .. }) {
        // No legend rows drawn until the legend says otherwise, so a frame
        // with no room for any leaves none to focus.
        view.record_legend_targets(Rect::new(body.x, body.y, body.width, 0));
    }
    if body.is_empty() {
        return;
    }
    // The wheel steps ranges over where the chart stands, drawn or not, so a
    // range with nothing to draw can still be stepped past.
    view.record_chart_target(body);
    match view.content() {
        MetricsContent::Reading => {
            buffer.set_string(body.x, body.y, "Reading Usage Metrics…", subdued);
        }
        MetricsContent::Refused(refusal) => {
            let heading = match refusal {
                UsageMetricsReadRefusal::Refused(_) => "The Server refused the Usage Metrics read",
                UsageMetricsReadRefusal::Failed(_) => "Usage Metrics could not be read",
            };
            buffer.set_string(
                body.x,
                body.y,
                heading,
                theme.feedback.error.add_modifier(Modifier::BOLD),
            );
            let reason = truncate_to_width(refusal.reason(), usize::from(body.width));
            buffer.set_string(body.x, body.y + 1, reason, theme.feedback.error);
        }
        MetricsContent::Report { buckets, report } => {
            if chart::holds_nothing(report) {
                // Only All time's headline holds what its chart cannot: Turns
                // stored before Suru recorded when they ran.
                let note = if headed && range == MetricsRange::AllTime {
                    UNCHARTED
                } else {
                    range.no_usage()
                };
                let note = truncate_to_width(note, usize::from(body.width));
                buffer.set_string(body.x, body.y, note, subdued);
                return;
            }
            let series = chart::series(report, measure, theme);
            // Hiding a series changes only the chart: the legend's heading,
            // figures, and shares stay those of every series, hidden or not,
            // as the headline figures above it will.
            let across = Legend::of_range(&series, measure, range, &report.across_buckets.total);
            let selected = view
                .selected_bucket()
                .and_then(|index| Legend::of_bucket(&series, measure, buckets, report, index));
            // Room is kept for a note either legend needs, so selecting a
            // bar never moves the chart.
            let noted = across.noted || selected.as_ref().is_some_and(|legend| legend.noted);
            let mixed = measure == MetricsMeasure::Tokens;
            let legend_rows = u16::try_from(series.len() + 1 + usize::from(noted))
                .unwrap_or(u16::MAX)
                .saturating_add(if mixed { MIX_ROWS + 1 } else { 0 });
            let chart_rows = body.height.saturating_sub(legend_rows + 1).min(24);
            let chart_area = Rect::new(body.x, body.y, body.width, chart_rows);
            view.record_chart_target(chart_area);
            // Hidden series drop out of the stacks, so the axis rises only as
            // far as the series still shown reach.
            let stacked: Vec<Series> = series
                .iter()
                .filter(|series| !view.is_hidden(&series.id))
                .cloned()
                .collect();
            if let Some(bars) = chart::render_chart(
                buffer,
                chart_area,
                buckets,
                &stacked,
                measure,
                view.selected_bucket(),
                theme,
            ) {
                view.record_bar_targets(bars);
            }
            let legend = Rect::new(
                body.x,
                body.y + chart_rows + 1,
                body.width,
                body.height.saturating_sub(chart_rows + 1),
            );
            let shown = selected.unwrap_or(across);
            render_legend(frame, legend, &series, &shown, view, theme);
            if mixed {
                // Under the legend, a blank row apart, reading what it reads.
                let after = u16::try_from(series.len() + 2 + usize::from(noted))
                    .unwrap_or(u16::MAX)
                    .min(legend.height);
                let mix = Rect::new(
                    legend.x,
                    legend.y + after,
                    legend.width,
                    (legend.height - after).min(MIX_ROWS),
                );
                headline::render_mix(frame.buffer_mut(), mix, &shown.aggregate, theme);
            }
        }
    }
}

/// Every tab from `x` along row `y`, each a label with a column either side
/// that a pointer can choose it by, recorded by its place in `tabs`, the
/// chosen one drawn as a filled tab in those columns, so no label moves as
/// the choice does. Answers the column after the last.
fn render_tabs<const N: usize>(
    buffer: &mut ratatui::buffer::Buffer,
    mut x: u16,
    y: u16,
    tabs: [(&str, bool); N],
    record: impl Fn(usize, Rect),
    theme: &Theme,
) -> u16 {
    let accent = theme.accent.primary;
    for (place, (label, chosen)) in tabs.into_iter().enumerate() {
        let label_width = u16::try_from(label.width()).unwrap_or(u16::MAX);
        if chosen {
            buffer.set_string(x, y, "▐", accent);
            buffer.set_string(
                x + 1,
                y,
                label,
                accent.add_modifier(Modifier::BOLD | Modifier::REVERSED),
            );
            buffer.set_string(x + 1 + label_width, y, "▌", accent);
        } else {
            buffer.set_string(x + 1, y, label, theme.text.subdued);
        }
        let width = label_width + 2;
        record(place, Rect::new(x, y, width, 1));
        x += width + TAB_GAP;
    }
    x - TAB_GAP
}

/// What the legend reads: the range's figures, or, where a bar is selected,
/// that bucket's in their place, so the legend is the bar's tooltip.
struct Legend {
    heading: String,
    /// Each series' figure, and whether it says less than was consumed, in
    /// the series' order.
    figures: Vec<(u64, bool)>,
    /// The whole each figure's share is of.
    whole: u64,
    /// Whether the figures count more than the whole, as Sessions under
    /// several keys do, so a note must say why.
    noted: bool,
    /// The figures of the whole in every measure, which the Usage mix strip
    /// reads.
    aggregate: UsageMetricsAggregate,
}

impl Legend {
    /// A legend of `figures` over `whole`, headed by `name` and that whole,
    /// then, where `counts`, the distinct Sessions that worked in it —
    /// never the series' Sessions added, which count a Session once under
    /// each key — unless the measure's whole already counts them, and its
    /// Turns.
    fn new(
        name: String,
        figures: Vec<(u64, bool)>,
        measure: MetricsMeasure,
        whole: &UsageMetricsAggregate,
        counts: bool,
    ) -> Self {
        let total = measure.value(whole);
        let mut heading = format!(
            "{name} · {}",
            measure.total(total, measure.is_partial(whole))
        );
        if counts {
            if measure != MetricsMeasure::Sessions {
                heading.push_str(" · ");
                heading.push_str(&MetricsMeasure::Sessions.total(whole.sessions, false));
            }
            heading.push_str(" · ");
            heading.push_str(&match whole.turns {
                1 => "1 Turn".to_owned(),
                turns => format!("{} Turns", MetricsMeasure::Sessions.format(turns)),
            });
        }
        Self {
            heading,
            noted: overcounts(&figures, measure, total),
            figures,
            whole: total,
            aggregate: *whole,
        }
    }

    /// The legend of the range taken together, headed by its name.
    fn of_range(
        series: &[Series],
        measure: MetricsMeasure,
        range: MetricsRange,
        whole: &UsageMetricsAggregate,
    ) -> Self {
        let figures = series
            .iter()
            .map(|series| (series.total, series.is_partial))
            .collect();
        Self::new(range.title().to_owned(), figures, measure, whole, false)
    }

    /// The legend of the `index`-th of `buckets` alone, headed by its local
    /// date or hour, its Sessions, and its Turns, where the report holds
    /// that bucket.
    fn of_bucket(
        series: &[Series],
        measure: MetricsMeasure,
        buckets: &RangeBuckets,
        report: &UsageMetricsReport,
        index: usize,
    ) -> Option<Self> {
        let bucket = buckets.buckets.get(index)?;
        let whole = &report.buckets.get(index)?.total;
        let figures = series
            .iter()
            .map(|series| {
                (
                    series.values.get(index).copied().unwrap_or(0),
                    series.partials.get(index).copied().unwrap_or(false),
                )
            })
            .collect();
        let name = chart::bucket_heading(bucket, buckets.unit);
        Some(Self::new(name, figures, measure, whole, true))
    }
}

/// The legend's heading, then one row per series in the chart's order —
/// ranked by Cost over the range whatever the measure or bucket, Other last —
/// each its colour, label, figure, share of the legend's whole, and a bar
/// proportional to the largest figure shown, a hidden series' row subdued.
/// Where the figures count more than the whole, a note under them says why.
fn render_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[Series],
    legend: &Legend,
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let measure = view.measure();
    let drawn = u16::try_from(series.len())
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(1));
    view.record_legend_targets(Rect::new(area.x, area.y + 1, area.width, drawn));
    if area.height == 0 {
        return;
    }
    let total = legend.whole;
    buffer.set_string(
        area.x,
        area.y,
        &legend.heading,
        theme.text.primary.add_modifier(Modifier::BOLD),
    );
    let label_width = series
        .iter()
        .map(|series| series.label.width())
        .max()
        .unwrap_or(0)
        .min(LEGEND_LABEL);
    let values: Vec<String> = legend
        .figures
        .iter()
        .map(|(figure, is_partial)| measure.figure(*figure, *is_partial))
        .collect();
    let value_width = values.iter().map(|value| value.width()).max().unwrap_or(0);
    // Series stand in the same order whatever the measure, so the largest
    // in this one need not be the first.
    let largest = legend
        .figures
        .iter()
        .map(|(figure, _)| *figure)
        .max()
        .unwrap_or(0)
        .max(1);
    let focused = view.focused_row();
    let fixed = 2 + label_width + 2 + value_width + 2 + 4 + 2;
    let bar_room = usize::from(area.width)
        .saturating_sub(fixed)
        .min(usize::from(LEGEND_BAR));
    for (row, ((series, value), (figure, _))) in
        series.iter().zip(&values).zip(&legend.figures).enumerate()
    {
        let Some(y) = u16::try_from(row + 1)
            .ok()
            .map(|row| area.y + row)
            .filter(|y| *y < area.bottom())
        else {
            break;
        };
        // A hidden series keeps its row and figures, so it can be found and
        // shown again, its swatch and bar hollow and its words subdued: no
        // modifier a terminal may ignore says it alone.
        let hidden = view.is_hidden(&series.id);
        let glyph = if hidden { "░" } else { "█" };
        let swatch = Style::default().fg(series.color);
        buffer.set_string(area.x, y, glyph, swatch);
        let focused = focused == Some(FocusedRow::Series(row));
        let text_style = if focused {
            theme.selection.focused
        } else {
            theme.text.primary
        };
        let text_style = if hidden {
            text_style.patch(theme.text.subdued)
        } else {
            text_style
        };
        if focused && let Some(x) = area.x.checked_sub(1) {
            // Marked in the inset before the swatch, so no row moves.
            buffer.set_string(x, y, "›", text_style);
        }
        // Padded by the columns it is drawn in, not its characters, so a
        // wide glyph in one label leaves the figures beside every row aligned.
        let label = truncate_to_width(&series.label, label_width);
        let padding = " ".repeat(label_width.saturating_sub(label.width()));
        let text = format!(
            "{label}{padding}  {value:>value_width$}  {share:>4}",
            share = share(*figure, total),
        );
        buffer.set_string(area.x + 2, y, &text, text_style);
        let bar = usize::try_from(u128::from(*figure) * bar_room as u128 / u128::from(largest))
            .unwrap_or(0);
        let bar = if *figure > 0 { bar.max(1) } else { 0 };
        let bar_x = area.x + u16::try_from(fixed).unwrap_or(u16::MAX);
        if bar > 0 && bar_x < area.right() {
            buffer.set_string(bar_x, y, glyph.repeat(bar), swatch);
        }
    }
    let note_y = u16::try_from(series.len() + 1)
        .ok()
        .map(|row| area.y.saturating_add(row))
        .filter(|y| *y < area.bottom());
    if legend.noted
        && let Some(y) = note_y
    {
        let note = overlap_note(view.breakdown());
        let note = truncate_to_width(&note, usize::from(area.width));
        buffer.set_string(area.x, y, note, theme.text.subdued);
    }
}

/// Why the legend's series can count more Sessions than its heading does,
/// in the words of the breakdown that splits them.
fn overlap_note(breakdown: UsageMetricsBreakdown) -> String {
    format!(
        "A Session that used several {} counts once for each",
        breakdown.plural()
    )
}

/// Whether `figures` count more than `whole` does, as a measure that does
/// not add up can: a Session that worked under several keys counts once
/// under each and once in the whole.
fn overcounts(figures: &[(u64, bool)], measure: MetricsMeasure, whole: u64) -> bool {
    let counted = figures
        .iter()
        .map(|(figure, _)| *figure)
        .fold(0, u64::saturating_add);
    !measure.adds_up() && counted > whole
}

/// What the view says of figures a refresh could not replace.
fn stale_note(refusal: &UsageMetricsReadRefusal) -> String {
    match refusal {
        UsageMetricsReadRefusal::Refused(reason) => format!("Not refreshed: {reason}"),
        UsageMetricsReadRefusal::Failed(reason) => format!("Could not refresh: {reason}"),
    }
}

/// `part` of `whole` as a whole percentage, with a small part that rounds
/// to nothing still shown as there.
fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return String::new();
    }
    let percent = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    if percent == 0 && part > 0 {
        "<1%".to_owned()
    } else {
        format!("{percent}%")
    }
}

#[cfg(test)]
mod tests {
    use super::share;

    #[test]
    fn a_share_is_a_whole_percentage_and_a_sliver_still_shows() {
        assert_eq!(share(58, 100), "58%");
        assert_eq!(share(2, 3), "67%");
        assert_eq!(share(1, 1_000), "<1%");
        assert_eq!(share(0, 1_000), "0%");
    }
}
