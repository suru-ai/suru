//! Draws the Insights tab beneath the range bar: its panels in a grid of two
//! columns, each bordered and titled, the focused one's border in the
//! accent; or what stands in for them while the read is on its way, or
//! where it was refused.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::Cost, theme::Theme};

use super::{
    super::{UsageMetricsReadRefusal, calendar::MetricsRange, chart::MetricsMeasure},
    InsightsContent, InsightsPanel, InsightsView, KeptReport, approvals, cache, compactions,
    delegation, heatmap, tools, turns, workspaces,
};
use crate::{protocol::Outlook, tui::slots::truncate_to_width};

/// Columns between the grid's two columns.
const COLUMN_GAP: u16 = 2;

/// Draws the tab's report for `range` from `origin` in `body`.
pub(in crate::tui) fn render(
    buffer: &mut Buffer,
    body: Rect,
    view: &InsightsView,
    origin: &Outlook,
    range: MetricsRange,
    theme: &Theme,
) {
    view.forget_targets();
    match view.content(origin, range) {
        InsightsContent::Reading => {
            buffer.set_string(body.x, body.y, "Reading Insights…", theme.text.subdued);
        }
        InsightsContent::Refused(refusal) => {
            let heading = match refusal {
                UsageMetricsReadRefusal::Refused(_) => "The Server refused the Insights read",
                UsageMetricsReadRefusal::Failed(_) => "Insights could not be read",
            };
            buffer.set_string(
                body.x,
                body.y,
                heading,
                theme.feedback.error.add_modifier(Modifier::BOLD),
            );
            if body.height > 1 {
                let reason = truncate_to_width(refusal.reason(), usize::from(body.width));
                buffer.set_string(body.x, body.y + 1, reason, theme.feedback.error);
            }
        }
        InsightsContent::Report(kept) => {
            render_grid(buffer, body, view, kept, origin, range, theme);
        }
    }
}

/// The panels row by row across two columns where [`InsightsPanel::GRID`]
/// places them, each row as tall as its tallest panel, and none drawn past
/// the foot of `body`.
fn render_grid(
    buffer: &mut Buffer,
    body: Rect,
    view: &InsightsView,
    kept: &KeptReport,
    origin: &Outlook,
    range: MetricsRange,
    theme: &Theme,
) {
    let column_width = body.width.saturating_sub(COLUMN_GAP) / 2;
    let mut y = body.y;
    for row in InsightsPanel::GRID {
        let tallest = row.iter().flatten().map(|panel| height(*panel)).max();
        let tallest = tallest.unwrap_or(0);
        if y + tallest > body.bottom() {
            break;
        }
        for (column, panel) in (0u16..).zip(row) {
            let Some(panel) = panel else {
                continue;
            };
            let x = body.x + column * (column_width + COLUMN_GAP);
            let area = Rect::new(x, y, column_width, height(panel));
            render_panel(buffer, area, panel, view, kept, origin, range, theme);
        }
        y += tallest + 1;
    }
}

/// How many rows `panel` takes, its border included.
fn height(panel: InsightsPanel) -> u16 {
    let rows = match panel {
        InsightsPanel::WhenYouWork => heatmap::ROWS,
        InsightsPanel::Turns => turns::ROWS,
        InsightsPanel::Tools => tools::ROWS,
        InsightsPanel::Compactions => compactions::ROWS,
        InsightsPanel::Cache => cache::ROWS,
        InsightsPanel::Approvals => approvals::ROWS,
        InsightsPanel::Delegation => delegation::ROWS,
        InsightsPanel::Workspaces => workspaces::ROWS,
    };
    rows + 2
}

#[allow(clippy::too_many_arguments)]
fn render_panel(
    buffer: &mut Buffer,
    area: Rect,
    panel: InsightsPanel,
    view: &InsightsView,
    kept: &KeptReport,
    origin: &Outlook,
    range: MetricsRange,
    theme: &Theme,
) {
    let focused = view.focused() == Some(panel);
    let border = if focused {
        theme.accent.primary
    } else {
        theme.border.subdued
    };
    let title = if focused {
        theme.accent.primary.add_modifier(Modifier::BOLD)
    } else {
        theme.text.primary.add_modifier(Modifier::BOLD)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(format!(" {} ", panel.title()))
        .title_style(title);
    let inner = block.inner(area);
    block.render(area, buffer);
    view.record_panel_target(panel, area);
    // A column of room either side of the figures, inside the border.
    let inner = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let selected = view.selected_item().filter(|_| focused);
    match panel {
        InsightsPanel::WhenYouWork => {
            heatmap::render(buffer, inner, &kept.heatmap, range, selected, view, theme);
        }
        InsightsPanel::Turns => {
            turns::render(
                buffer,
                inner,
                &kept.report.turns,
                range,
                selected,
                view,
                theme,
            );
        }
        InsightsPanel::Tools => {
            tools::render(
                buffer,
                inner,
                &kept.report.tools,
                range,
                selected,
                view,
                theme,
            );
        }
        InsightsPanel::Compactions => {
            compactions::render(buffer, inner, &kept.report.compactions, range, theme);
        }
        InsightsPanel::Cache => {
            cache::render(buffer, inner, &kept.report.cache, range, theme);
        }
        InsightsPanel::Approvals => {
            approvals::render(buffer, inner, &kept.report.approvals, range, theme);
        }
        InsightsPanel::Delegation => {
            delegation::render(buffer, inner, &kept.report.delegation, range, theme);
        }
        InsightsPanel::Workspaces => {
            let content = view.workspaces_content(origin, range);
            workspaces::render(buffer, inner, content, range, selected, view, theme);
        }
    }
}

/// Draws `spans` one after another along the row `y` of `area`, each in its
/// own style, cutting short whatever runs past its right edge.
pub(super) fn render_spans(
    buffer: &mut Buffer,
    area: Rect,
    y: u16,
    spans: impl IntoIterator<Item = (String, Style)>,
) {
    let mut x = area.x;
    for (text, style) in spans {
        let left = usize::from(area.right().saturating_sub(x));
        let text = truncate_to_width(&text, left);
        buffer.set_string(x, y, &text, style);
        x += u16::try_from(text.width()).unwrap_or(u16::MAX);
    }
}

/// The glyphs a bar's last cell is drawn in, an eighth of a cell full to
/// seven eighths.
const EIGHTHS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// A bar as long, in eighths of `width` cells, as `part` is of `whole`, to
/// the nearest eighth: any part at all at least an eighth long.
pub(super) fn bar(part: u64, whole: u64, width: usize) -> String {
    if part == 0 || whole == 0 || width == 0 {
        return String::new();
    }
    let span = u128::try_from(width).unwrap_or(u128::MAX).saturating_mul(8);
    let eighths = (u128::from(part) * span + u128::from(whole) / 2) / u128::from(whole);
    let eighths = usize::try_from(eighths.clamp(1, span)).unwrap_or(usize::MAX);
    let mut bar = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        bar.push_str(EIGHTHS[eighths % 8 - 1]);
    }
    bar
}

/// A Cost as a panel states it: its amount, `+` where it is partial; or,
/// where it counts no amount, whether that is for want of work or of a
/// price, never a zero nobody measured.
pub(super) fn stated_cost(cost: Option<Cost>, is_partial: bool) -> String {
    match (cost, is_partial) {
        (Some(cost), is_partial) => MetricsMeasure::Cost.figure(cost.nano_usd(), is_partial),
        (None, true) => "unpriced".to_owned(),
        (None, false) => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::bar;

    #[test]
    fn a_bar_is_as_long_as_its_share_to_the_nearest_eighth_and_a_sliver_still_shows() {
        assert_eq!(bar(1, 2, 4), "██");
        assert_eq!(bar(3, 16, 4), "▊", "six of four cells' eighths");
        assert_eq!(bar(1, 1_000, 4), "▏");
        assert_eq!(bar(0, 1_000, 4), "");
        assert_eq!(bar(1_000, 1_000, 4), "████");
    }
}
