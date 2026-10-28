//! The Workspaces panel: the Workspaces the range's work was done in,
//! ranked by Cost as Overview ranks its Workspace keys, each with its Cost —
//! `+` where that is partial — and a bar as long beside the largest's as its
//! Cost is. Rows past the panel's height fold into a last "(N more)" row
//! stating what they cost together, which the arrows and the wheel scroll
//! into; opening a row shows that Workspace alone on Overview.
//!
//! The rows come from the metrics read Overview makes, asked for in one
//! bucket over the range by Workspace, so the panel needs no read of its
//! own.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{Cost, UsageMetricsAggregate, UsageMetricsKey, UsageMetricsReport},
    theme::Theme,
};

use super::{
    super::{
        super::{slots::truncate_to_width, usage::grouped_count},
        UsageMetricsReadRefusal,
        calendar::MetricsRange,
        chart,
    },
    InsightsPanel, InsightsView, RowWindow, WorkspacesContent,
    view::{bar, nothing, render_spans, stated_cost},
};

/// How many lines the panel has for its Workspaces, the fold among them.
pub(super) const SHOWN: usize = 5;

/// The rows the panel draws inside its border: its Workspaces.
pub(super) const ROWS: u16 = SHOWN as u16;

/// How wide a Cost stands, wide enough for thousands of dollars marked
/// partial.
const COST_WIDTH: usize = 10;

/// One Workspace's row: its key, the name it is shown by, and its figures
/// over the range.
#[derive(Clone, Debug)]
pub(in crate::tui) struct WorkspaceRow {
    pub(super) key: UsageMetricsKey,
    label: String,
    aggregate: UsageMetricsAggregate,
}

/// `report`'s Workspaces in the order Overview ranks them: by Cost, then
/// by tokens, then by Sessions, ties settled by name.
pub(super) fn rows(report: &UsageMetricsReport) -> Vec<WorkspaceRow> {
    chart::ranked_keys(report)
        .into_iter()
        .map(|(label, aggregate)| WorkspaceRow {
            key: label.key.clone(),
            label: label.label.clone(),
            aggregate,
        })
        .collect()
}

/// How many items the panel offers for selection: a Workspace apiece.
pub(super) fn items(rows: &[WorkspaceRow]) -> usize {
    rows.len()
}

/// What `rows` cost together, in nano-dollars, and whether any of that is
/// partial.
fn cost_of(rows: &[WorkspaceRow]) -> (Option<Cost>, bool) {
    let cost = rows
        .iter()
        .filter_map(|row| row.aggregate.cost)
        .reduce(Cost::saturating_add);
    let partial = rows.iter().any(|row| row.aggregate.is_cost_partial);
    (cost, partial)
}

/// Draws the panel's rows in `area`, inside its border, scrolled as `view`
/// has it, and the Workspace `selected` marked.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    content: WorkspacesContent<'_>,
    range: MetricsRange,
    selected: Option<usize>,
    view: &InsightsView,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let rows = match content {
        WorkspacesContent::Reading => {
            buffer.set_string(area.x, area.y, "Reading Workspaces…", subdued);
            return;
        }
        WorkspacesContent::Refused(refusal) => {
            let heading = match refusal {
                UsageMetricsReadRefusal::Refused(_) => "The Server refused the Workspace read",
                UsageMetricsReadRefusal::Failed(_) => "Workspaces could not be read",
            };
            let style = theme.feedback.error;
            buffer.set_string(area.x, area.y, truncate_to_width(heading, room), style);
            let reason = truncate_to_width(refusal.reason(), room);
            buffer.set_string(area.x, area.y + 1, reason, style);
            return;
        }
        WorkspacesContent::Rows([]) => {
            let note = nothing(room, "No work", range);
            buffer.set_string(area.x, area.y, note, subdued);
            return;
        }
        WorkspacesContent::Rows(rows) => rows,
    };
    let bar_width = room.saturating_sub(COST_WIDTH + 2) / 2;
    let label_width = room.saturating_sub(COST_WIDTH + 2 + bar_width + 1);
    // The first row costs the most, so every bar is drawn beside its.
    let largest = rows[0].aggregate.cost.map_or(0, Cost::nano_usd);
    let mut line = |y: u16, label: &str, (cost, partial): (Option<Cost>, bool), style: Style| {
        let label = truncate_to_width(label, label_width);
        let pad = label_width.saturating_sub(label.width());
        let lead = format!(
            "{label}{:pad$} {:>COST_WIDTH$}  ",
            "",
            stated_cost(cost, partial),
        );
        let part = cost.map_or(0, Cost::nano_usd);
        render_spans(
            buffer,
            area,
            y,
            [
                (lead, style),
                (bar(part, largest, bar_width), theme.accent.primary),
            ],
        );
    };
    let panel = InsightsPanel::Workspaces;
    let window = RowWindow::new(rows.len(), SHOWN, view.scrolled(panel));
    let shown = rows
        .iter()
        .enumerate()
        .skip(window.first)
        .take(window.shown);
    for (y, (place, row)) in (area.y..).zip(shown) {
        let style = if selected == Some(place) {
            theme.accent.primary.add_modifier(Modifier::REVERSED)
        } else {
            theme.text.primary
        };
        let cost = (row.aggregate.cost, row.aggregate.is_cost_partial);
        line(y, &row.label, cost, style);
    }
    if window.folded > 0 {
        let y = area.y + u16::try_from(window.shown).unwrap_or(u16::MAX);
        let folded = u64::try_from(window.folded).unwrap_or(u64::MAX);
        let fold = format!("({} more)", grouped_count(folded));
        line(
            y,
            &fold,
            cost_of(&rows[window.first + window.shown..]),
            subdued,
        );
    }
    let shown = u16::try_from(window.shown).unwrap_or(u16::MAX);
    view.record_item_targets(
        panel,
        Rect::new(area.x, area.y, area.width, shown),
        area.width,
        window.first,
    );
}
