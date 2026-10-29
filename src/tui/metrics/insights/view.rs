//! Draws the Insights tab beneath the range bar: its panels in the columns
//! [`InsightsPanel::LAYOUT`] stands them in, each bordered and titled, the
//! focused one's border in the accent; or what stands in for them while the
//! read is on its way, or where it was refused.
//!
//! Columns stand side by side where each is at least as wide as it says it
//! needs, and otherwise stack one after another in the order they are read,
//! any columns within them stacking too: so a frame too narrow for the
//! mockup's three columns beneath When you work keeps two, and one
//! narrower than the mockup stacks every panel in one column as wide as
//! the frame.
//!
//! The panels are laid out first, as tall as they need, then drawn whole
//! and shown through the body as through a window, scrolled as far down as
//! [`InsightsView::body_scroll`] says: wherever they run past the foot of
//! the frame — in one column or in the grid — every one of them can be
//! scrolled into view, and none is lost. What a pointer can act on is
//! recorded where it is shown, and nowhere it is scrolled out of view.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::CountedCost, theme::Theme};

use super::{
    super::{UsageMetricsReadRefusal, calendar::MetricsRange, chart::MetricsMeasure},
    Column, InsightsContent, InsightsPanel, InsightsView, KeptReport, Place, approvals, cache,
    compactions, delegation, heatmap, tools, turns, workspaces,
};
use crate::{
    protocol::Outlook,
    tui::{slots::truncate_to_width, usage::grouped_count},
};

/// Columns between two columns standing side by side.
const COLUMN_GAP: u16 = 2;

/// The narrowest a column stands beside another in where it says no more:
/// room for the briefest figures of the small panels, inside their borders.
pub(super) const BESIDE_WIDTH: u16 = 28;

/// How many of a panel's columns its border and the column of room either
/// side of its figures take.
const BORDERED: u16 = 4;

/// What every panel is drawn from.
struct Drawing<'a> {
    view: &'a InsightsView,
    kept: &'a KeptReport,
    origin: &'a Outlook,
    range: MetricsRange,
    theme: &'a Theme,
}

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
            let drawing = Drawing {
                view,
                kept,
                origin,
                range,
                theme,
            };
            render_report(buffer, body, &drawing);
        }
    }
}

/// Lays the panels out from the top of a body as wide as `body`, draws
/// them whole, and shows those rows of them the body's scroll reaches,
/// with a word at its foot where more stand above or below.
fn render_report(buffer: &mut Buffer, body: Rect, drawing: &Drawing<'_>) {
    let view = drawing.view;
    let mut laid = Vec::with_capacity(InsightsPanel::COUNT);
    let height = lay_out(
        InsightsPanel::LAYOUT,
        (body.x, 0, body.width),
        false,
        drawing.kept,
        &mut laid,
    );
    let scroll = view.settle_body_scroll(height.saturating_sub(body.height));
    let mut drawn = Buffer::empty(Rect::new(body.x, 0, body.width, height));
    for (panel, area) in laid {
        render_panel(&mut drawn, area, panel, drawing);
    }
    for row in 0..body.height.min(height - scroll) {
        for x in body.x..body.right() {
            buffer[(x, body.y + row)] = drawn[(x, scroll + row)].clone();
        }
    }
    view.show_targets(body, scroll, height);
    render_scroll_hint(buffer, body, scroll, height, drawing.theme);
}

/// Says at the foot of `body`, beneath the panels, which way more of them
/// stand and the key that pages to them: none where all are in view.
fn render_scroll_hint(buffer: &mut Buffer, body: Rect, scroll: u16, height: u16, theme: &Theme) {
    let above = scroll > 0;
    let below = scroll + body.height < height;
    let hint = match (above, below) {
        (false, false) => return,
        (true, false) => "↑ PgUp",
        (false, true) => "↓ PgDn",
        (true, true) => "↑ PgUp  ↓ PgDn",
    };
    let width = u16::try_from(hint.width()).unwrap_or(u16::MAX);
    let x = body.right().saturating_sub(width);
    buffer.set_string(x, body.bottom(), hint, theme.text.subdued);
}

/// Lays `place` out from row `y`, `width` columns wide from column `x`,
/// adding where each of its panels stands to `laid`, and answers how many
/// rows it takes. Where `stacked`, columns that stacked hold it, and any
/// within it stack too, so that a frame too narrow for two columns lays
/// every panel out in one.
fn lay_out(
    place: Place,
    (x, y, width): (u16, u16, u16),
    stacked: bool,
    kept: &KeptReport,
    laid: &mut Vec<(InsightsPanel, Rect)>,
) -> u16 {
    match place {
        Place::Panel(panel) => {
            let height = height(panel, width, kept);
            laid.push((panel, Rect::new(x, y, width, height)));
            height
        }
        Place::Beside(columns) => {
            let widths = beside(width, columns).filter(|_| !stacked);
            let Some(widths) = widths else {
                // Too narrow to stand side by side, the columns stack.
                let places = columns
                    .iter()
                    .flat_map(|column| column.places.iter().copied());
                return lay_out_stack(places, (x, y, width), true, kept, laid);
            };
            let mut x = x;
            let mut tallest = 0;
            for (column, width) in columns.iter().zip(widths) {
                let places = column.places.iter().copied();
                let height = lay_out_stack(places, (x, y, width), false, kept, laid);
                tallest = tallest.max(height);
                x += width + COLUMN_GAP;
            }
            tallest
        }
    }
}

/// Lays `places` out one beneath another from row `y`, a blank row between
/// each, as [`lay_out`] lays each, and answers how many rows they take.
fn lay_out_stack(
    places: impl IntoIterator<Item = Place>,
    (x, y, width): (u16, u16, u16),
    stacked: bool,
    kept: &KeptReport,
    laid: &mut Vec<(InsightsPanel, Rect)>,
) -> u16 {
    let mut taken = None;
    for place in places {
        let at = taken.map_or(y, |taken| y + taken + 1);
        let height = lay_out(place, (x, at, width), stacked, kept, laid);
        taken = Some(at + height - y);
    }
    taken.unwrap_or(0)
}

/// How wide each of `columns` stands in `width`, a gap between each, by
/// their weights, the last taking what rounding leaves; none where any
/// would be narrower than it stands beside another in.
fn beside(width: u16, columns: &[Column]) -> Option<Vec<u16>> {
    let gaps = COLUMN_GAP * u16::try_from(columns.len().saturating_sub(1)).ok()?;
    let room = u32::from(width.checked_sub(gaps)?);
    let weights: u32 = columns.iter().map(|column| u32::from(column.weight)).sum();
    let mut left = room;
    let mut widths = Vec::with_capacity(columns.len());
    for (place, column) in columns.iter().enumerate() {
        let share = if place + 1 == columns.len() {
            left
        } else {
            room * u32::from(column.weight) / weights.max(1)
        };
        left -= share;
        widths.push(u16::try_from(share).ok()?);
    }
    widths
        .iter()
        .zip(columns)
        .all(|(width, column)| *width >= column.narrowest)
        .then_some(widths)
}

/// How many rows `panel` takes `width` columns wide, its border included.
fn height(panel: InsightsPanel, width: u16, kept: &KeptReport) -> u16 {
    let room = usize::from(width.saturating_sub(BORDERED));
    let rows = match panel {
        InsightsPanel::WhenYouWork => heatmap::ROWS,
        InsightsPanel::Turns => turns::ROWS,
        InsightsPanel::Tools => tools::ROWS,
        InsightsPanel::Compactions => compactions::ROWS,
        InsightsPanel::Cache => cache::ROWS,
        InsightsPanel::Approvals => approvals::rows(&kept.report.approvals, room),
        InsightsPanel::Delegation => delegation::ROWS,
        InsightsPanel::Workspaces => workspaces::ROWS,
    };
    rows + 2
}

fn render_panel(buffer: &mut Buffer, area: Rect, panel: InsightsPanel, drawing: &Drawing<'_>) {
    let Drawing {
        view,
        kept,
        origin,
        range,
        theme,
    } = *drawing;
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
        area.width.saturating_sub(BORDERED),
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

/// The fullest of `forms`, given fullest first, that fits in `room`
/// columns: the briefest, cut short, where none does. A panel says what it
/// can in the width its column gives it.
pub(super) fn fitting(room: usize, forms: impl IntoIterator<Item = String>) -> String {
    let mut briefest = String::new();
    for form in forms {
        if form.width() <= room {
            return form;
        }
        briefest = form;
    }
    truncate_to_width(&briefest, room)
}

/// What a panel says where the range holds nothing for it: no `what`
/// within the range, or, where that does not fit, no `what` at all.
pub(super) fn nothing(room: usize, what: &str, range: MetricsRange) -> String {
    fitting(
        room,
        [format!("{what} {}", range.within()), what.to_owned()],
    )
}

/// Each of `tallies` — a glyph, a count, what the count is of, and the
/// glyph's style — as spans, the counts in `figures`' style, two columns
/// between one and the next, and each count named where `named`: how
/// Turns ended, or how Approvals were decided.
pub(super) fn tallies(
    tallies: &[(&str, u64, &str, Style)],
    named: bool,
    figures: Style,
) -> Vec<(String, Style)> {
    tallies
        .iter()
        .enumerate()
        .flat_map(|(place, (glyph, count, name, style))| {
            let gap = if place == 0 { "" } else { "  " };
            let count = grouped_count(*count);
            let figure = if named {
                format!("{count} {name}")
            } else {
                count
            };
            [(format!("{gap}{glyph} "), *style), (figure, figures)]
        })
        .collect()
}

/// Draws the fullest of `forms` — each spans in their own styles, given
/// fullest first — that fits along the row `y` of `area`, as
/// [`render_spans`] does: the briefest, cut short, where none does.
pub(super) fn render_fitting_spans(
    buffer: &mut Buffer,
    area: Rect,
    y: u16,
    forms: impl IntoIterator<Item = Vec<(String, Style)>>,
) {
    let room = usize::from(area.width);
    let mut chosen = Vec::new();
    for form in forms {
        let fits = form.iter().map(|(text, _)| text.width()).sum::<usize>() <= room;
        chosen = form;
        if fits {
            break;
        }
    }
    render_spans(buffer, area, y, chosen);
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
pub(super) fn stated_cost(cost: CountedCost) -> String {
    match (cost.amount, cost.is_partial) {
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
