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

use crate::theme::Theme;

use super::{
    super::{UsageMetricsReadRefusal, calendar::MetricsRange},
    InsightsContent, InsightsPanel, InsightsView, KeptReport, approvals, cache, compactions,
    delegation, heatmap, tools, turns,
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
        InsightsContent::Report(kept) => render_grid(buffer, body, view, kept, range, theme),
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
            render_panel(buffer, area, panel, view, kept, range, theme);
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
    };
    rows + 2
}

fn render_panel(
    buffer: &mut Buffer,
    area: Rect,
    panel: InsightsPanel,
    view: &InsightsView,
    kept: &KeptReport,
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
