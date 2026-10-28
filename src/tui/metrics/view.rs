//! Draws the Usage Metrics view over the whole frame: a title bar with its
//! close control, the range bar and the measures beside it, then the chart
//! and its legend, or what stands in for them while a read is on its way,
//! when it was refused, or when the range holds no Usage.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{UsageMetricsAggregate, UsageMetricsBreakdown},
    theme::Theme,
};

use super::{
    super::slots::truncate_to_width,
    BREAKDOWNS, MetricsContent, MetricsView, OfferedBreakdown, UsageMetricsReadRefusal,
    calendar::{BucketUnit, CalendarBucket, MetricsRange},
    chart::{self, MetricsMeasure, Series},
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

    let body = Rect::new(
        area.x + INSET,
        area.y + 5,
        area.width.saturating_sub(INSET * 2),
        area.height.saturating_sub(6),
    );
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
                buffer.set_string(body.x, body.y, range.no_usage(), subdued);
                return;
            }
            let series = chart::series(report, measure, theme);
            let across = range_legend(&series, measure, view, &report.across_buckets.total);
            let selected = view.selected_bucket().and_then(|index| {
                let bucket = buckets.buckets.get(index)?;
                let figures = &report.buckets.get(index)?.total;
                Some(bucket_legend(
                    &series,
                    measure,
                    index,
                    buckets.unit,
                    bucket,
                    figures,
                ))
            });
            // Room is kept for a note either legend needs, so selecting a
            // bar never moves the chart.
            let noted = across.noted || selected.as_ref().is_some_and(|legend| legend.noted);
            let legend_rows =
                u16::try_from(series.len() + 1 + usize::from(noted)).unwrap_or(u16::MAX);
            let chart_rows = body.height.saturating_sub(legend_rows + 1).min(24);
            let chart_area = Rect::new(body.x, body.y, body.width, chart_rows);
            view.record_chart_target(chart_area);
            if let Some(bars) = chart::render_chart(
                buffer,
                chart_area,
                buckets,
                &series,
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
}

/// The legend of the range taken together, headed by its name and whole.
fn range_legend(
    series: &[Series],
    measure: MetricsMeasure,
    view: &MetricsView,
    whole: &UsageMetricsAggregate,
) -> Legend {
    let total = measure.value(whole);
    let figures: Vec<(u64, bool)> = series
        .iter()
        .map(|series| (series.total, series.is_partial))
        .collect();
    Legend {
        heading: format!(
            "{} · {}",
            view.range().title(),
            measure.total(total, measure.is_partial(whole))
        ),
        noted: overcounts(&figures, measure, total),
        figures,
        whole: total,
    }
}

/// The legend of the `index`-th bucket alone, headed by its local date or
/// hour, its whole, and the distinct Sessions that worked in it — never the
/// series' Sessions added, which count a Session once under each key.
fn bucket_legend(
    series: &[Series],
    measure: MetricsMeasure,
    index: usize,
    unit: BucketUnit,
    bucket: &CalendarBucket,
    whole: &UsageMetricsAggregate,
) -> Legend {
    let total = measure.value(whole);
    let figures: Vec<(u64, bool)> = series
        .iter()
        .map(|series| {
            (
                series.values.get(index).copied().unwrap_or(0),
                series.partials.get(index).copied().unwrap_or(false),
            )
        })
        .collect();
    let mut heading = format!(
        "{} · {}",
        chart::bucket_heading(bucket, unit),
        measure.total(total, measure.is_partial(whole))
    );
    // The Sessions measure's whole already counts them.
    if measure != MetricsMeasure::Sessions {
        heading.push_str(" · ");
        heading.push_str(&MetricsMeasure::Sessions.total(whole.sessions, false));
    }
    Legend {
        heading,
        noted: overcounts(&figures, measure, total),
        figures,
        whole: total,
    }
}

/// The legend's heading, then one row per series in the chart's order —
/// ranked by Cost over the range whatever the measure or bucket, Other last —
/// each its colour, label, figure, share of the legend's whole, and a bar
/// proportional to the largest figure shown. Where the figures count more
/// than the whole, a note under them says why.
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
        let swatch = Style::default().fg(series.color);
        buffer.set_string(area.x, y, "█", swatch);
        // Padded by the columns it is drawn in, not its characters, so a
        // wide glyph in one label leaves the figures beside every row aligned.
        let label = truncate_to_width(&series.label, label_width);
        let padding = " ".repeat(label_width.saturating_sub(label.width()));
        let text = format!(
            "{label}{padding}  {value:>value_width$}  {share:>4}",
            share = share(*figure, total),
        );
        buffer.set_string(area.x + 2, y, &text, theme.text.primary);
        let bar = usize::try_from(u128::from(*figure) * bar_room as u128 / u128::from(largest))
            .unwrap_or(0);
        let bar = if *figure > 0 { bar.max(1) } else { 0 };
        let bar_x = area.x + u16::try_from(fixed).unwrap_or(u16::MAX);
        if bar > 0 && bar_x < area.right() {
            buffer.set_string(bar_x, y, "█".repeat(bar), swatch);
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
