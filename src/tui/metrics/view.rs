//! Draws the Usage Metrics view over the whole frame: a title bar with its
//! close control, then the chart and its legend, or what stands in for them
//! while a read is on its way, when it was refused, or when the range holds
//! no Usage.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

use super::{
    super::slots::truncate_to_width,
    MetricsContent, MetricsView, UsageMetricsReadRefusal,
    chart::{self, Series},
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

pub(in crate::tui) fn render(frame: &mut Frame<'_>, view: &MetricsView, theme: &Theme) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
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
    let mut subtitle = format!("{} by Model · {}", measure.label(), range.label());
    if view.is_reading() && matches!(view.content(), MetricsContent::Report { .. }) {
        subtitle.push_str(" · Refreshing…");
    }
    let subtitle_width = u16::try_from(subtitle.width()).unwrap_or(u16::MAX);
    buffer.set_string(area.x + INSET, area.y + 3, &subtitle, subdued);
    // Figures that could not be read again stay, saying why beside them.
    if let Some(refusal) = view.stale_because() {
        let x = area.x + INSET + subtitle_width;
        let room = usize::from(area.right().saturating_sub(x + INSET));
        let note = truncate_to_width(&format!(" · {}", stale_note(refusal)), room);
        buffer.set_string(x, area.y + 3, note, theme.feedback.error);
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
            let series = chart::series(report, measure, theme);
            let total: u64 = series.iter().map(|series| series.total).sum();
            if total == 0 {
                buffer.set_string(
                    body.x,
                    body.y,
                    format!("No Usage in the {}.", range.label().to_lowercase()),
                    subdued,
                );
                return;
            }
            let legend_rows = u16::try_from(series.len() + 1).unwrap_or(u16::MAX);
            let chart_rows = body.height.saturating_sub(legend_rows + 1).min(24);
            chart::render_chart(
                buffer,
                Rect::new(body.x, body.y, body.width, chart_rows),
                buckets,
                range.unit(),
                &series,
                measure,
                theme,
            );
            let legend = Rect::new(
                body.x,
                body.y + chart_rows + 1,
                body.width,
                body.height.saturating_sub(chart_rows + 1),
            );
            render_legend(frame, legend, &series, total, view, theme);
        }
    }
}

/// A heading naming the range's whole, then one row per series from the
/// largest: its colour, label, figure, share, and a bar proportional to the
/// largest series.
fn render_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[Series],
    total: u64,
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let measure = view.measure();
    if area.height == 0 {
        return;
    }
    buffer.set_string(
        area.x,
        area.y,
        format!("{} · {}", view.range().label(), measure.total(total)),
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
        .map(|series| measure.format(series.total))
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
}

/// What the subtitle says of figures a refresh could not replace.
fn stale_note(refusal: &UsageMetricsReadRefusal) -> String {
    match refusal {
        UsageMetricsReadRefusal::Refused(reason) => format!("not refreshed: {reason}"),
        UsageMetricsReadRefusal::Failed(reason) => format!("could not refresh: {reason}"),
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
