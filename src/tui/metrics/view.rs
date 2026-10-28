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

use crate::{protocol::UsageMetricsAggregate, theme::Theme};

use super::{
    super::slots::truncate_to_width,
    MetricsContent, MetricsView, UsageMetricsReadRefusal,
    calendar::MetricsRange,
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
/// Why the legend's series can count more Sessions than its heading does.
const OVERLAP_NOTE: &str = "A Session that used several Models counts once for each";

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
    buffer.set_string(measures_end + CONTROL_GAP, controls_y, "by Model", subdued);
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
            let whole = &report.across_buckets.total;
            let noted = overcounts(&series, measure, whole);
            let legend_rows =
                u16::try_from(series.len() + 1 + usize::from(noted)).unwrap_or(u16::MAX);
            let chart_rows = body.height.saturating_sub(legend_rows + 1).min(24);
            let chart_area = Rect::new(body.x, body.y, body.width, chart_rows);
            view.record_chart_target(chart_area);
            chart::render_chart(buffer, chart_area, buckets, &series, measure, theme);
            let legend = Rect::new(
                body.x,
                body.y + chart_rows + 1,
                body.width,
                body.height.saturating_sub(chart_rows + 1),
            );
            render_legend(frame, legend, &series, whole, noted, view, theme);
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

/// A heading naming the range's whole, then one row per series from the
/// largest: its colour, label, figure, share, and a bar proportional to the
/// largest series. Where the series count more than the whole, `noted`, a
/// note under them says why.
fn render_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[Series],
    whole: &UsageMetricsAggregate,
    noted: bool,
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let measure = view.measure();
    if area.height == 0 {
        return;
    }
    let total = measure.value(whole);
    buffer.set_string(
        area.x,
        area.y,
        format!(
            "{} · {}",
            view.range().title(),
            measure.total(total, measure.is_partial(whole))
        ),
        theme.text.primary.add_modifier(Modifier::BOLD),
    );
    let label_width = series
        .iter()
        .map(|series| series.label.width())
        .max()
        .unwrap_or(0)
        .min(LEGEND_LABEL);
    let values: Vec<String> = series
        .iter()
        .map(|series| measure.figure(series.total, series.is_partial))
        .collect();
    let value_width = values.iter().map(|value| value.width()).max().unwrap_or(0);
    let largest = series.first().map_or(0, |series| series.total).max(1);
    let fixed = 2 + label_width + 2 + value_width + 2 + 4 + 2;
    let bar_room = usize::from(area.width)
        .saturating_sub(fixed)
        .min(usize::from(LEGEND_BAR));
    for (row, (series, value)) in series.iter().zip(&values).enumerate() {
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
            share = share(series.total, total),
        );
        buffer.set_string(area.x + 2, y, &text, theme.text.primary);
        let bar =
            usize::try_from(u128::from(series.total) * bar_room as u128 / u128::from(largest))
                .unwrap_or(0);
        let bar = if series.total > 0 { bar.max(1) } else { 0 };
        let bar_x = area.x + u16::try_from(fixed).unwrap_or(u16::MAX);
        if bar > 0 && bar_x < area.right() {
            buffer.set_string(bar_x, y, "█".repeat(bar), swatch);
        }
    }
    let note_y = u16::try_from(series.len() + 1)
        .ok()
        .map(|row| area.y.saturating_add(row))
        .filter(|y| *y < area.bottom());
    if noted && let Some(y) = note_y {
        let note = truncate_to_width(OVERLAP_NOTE, usize::from(area.width));
        buffer.set_string(area.x, y, note, theme.text.subdued);
    }
}

/// Whether `series` count more than `whole` does, as a measure that does not
/// add up can: a Session that worked under several keys counts once under
/// each and once in the whole.
fn overcounts(series: &[Series], measure: MetricsMeasure, whole: &UsageMetricsAggregate) -> bool {
    let counted = series
        .iter()
        .map(|series| series.total)
        .fold(0, u64::saturating_add);
    !measure.adds_up() && counted > measure.value(whole)
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
