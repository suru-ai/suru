//! The Tools panel: every Tool the range's Turns used, how often, and how
//! many of those uses failed — the most used first, or with `f` the most
//! failed first. Rows past the panel's height fold into a last "(N more)"
//! row, which the arrows and the wheel scroll into.

use std::{borrow::Cow, cmp::Reverse};

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{InsightsTool, ToolTally},
    theme::Theme,
};

use super::{
    super::{
        super::{slots::truncate_to_width, usage::grouped_count},
        calendar::MetricsRange,
    },
    InsightsPanel, InsightsView, RowWindow,
    view::nothing,
};

/// How many lines the panel has for its Tools, the fold among them.
pub(super) const SHOWN: usize = 5;

/// The rows the panel draws inside its border: its header, then its Tools.
pub(super) const ROWS: u16 = 1 + SHOWN as u16;

/// How wide the uses column is, wide enough for millions and its header.
const USES_WIDTH: usize = 9;

/// How wide the failed column is, wide enough for its header marked.
const FAILED_WIDTH: usize = 8;

/// How the panel orders its Tools.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::tui) enum ToolsSort {
    /// The most used first.
    #[default]
    Uses,
    /// The most failed first.
    Failures,
}

impl ToolsSort {
    pub(super) fn toggled(self) -> Self {
        match self {
            Self::Uses => Self::Failures,
            Self::Failures => Self::Uses,
        }
    }
}

/// How many items the panel offers for selection: a Tool apiece.
pub(super) fn items(tools: &[ToolTally]) -> usize {
    tools.len()
}

/// `tools` in the order `sort` lists them. The Server sends them the most
/// used first, which is that order as it stands; sorted by failures, those
/// as often failed are the most used first, and those alike in both keep
/// the Server's order.
fn ordered(tools: &[ToolTally], sort: ToolsSort) -> Vec<&ToolTally> {
    let mut ordered = tools.iter().collect::<Vec<_>>();
    if sort == ToolsSort::Failures {
        ordered.sort_by_key(|tool| Reverse((tool.failed, tool.uses)));
    }
    ordered
}

/// What the panel calls `tool`: a Tool an MCP server hosts by its server
/// and its name.
fn label(tool: &InsightsTool) -> Cow<'_, str> {
    match tool {
        InsightsTool::Command => Cow::Borrowed("Command"),
        InsightsTool::FileChange => Cow::Borrowed("File Change"),
        InsightsTool::Subagent => Cow::Borrowed("Subagent"),
        InsightsTool::Named { name, server: None } => Cow::Borrowed(name),
        InsightsTool::Named {
            name,
            server: Some(server),
        } => Cow::Owned(format!("{server} · {name}")),
    }
}

/// Draws the panel's table in `area`, inside its border, sorted and
/// scrolled as `view` has it, and the Tool `selected` marked.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    tools: &[ToolTally],
    range: MetricsRange,
    selected: Option<usize>,
    view: &InsightsView,
    theme: &Theme,
) {
    let sort = view.tools_sort();
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    if tools.is_empty() {
        let note = nothing(room, "No Tools used", range);
        buffer.set_string(area.x, area.y, note, subdued);
        return;
    }
    let label_width = room.saturating_sub(USES_WIDTH + 2 + FAILED_WIDTH + 1);
    let line = |name: &str, uses: &str, failed: &str| {
        let name = truncate_to_width(name, label_width);
        let pad = label_width.saturating_sub(name.width());
        format!(
            "{name}{:pad$} {uses:>USES_WIDTH$}  {failed:>FAILED_WIDTH$}",
            ""
        )
    };
    let (uses, failed) = match sort {
        ToolsSort::Uses => ("uses ↓", "failed"),
        ToolsSort::Failures => ("uses", "failed ↓"),
    };
    let header = line("Tool", uses, failed);
    buffer.set_string(area.x, area.y, truncate_to_width(&header, room), subdued);
    let ordered = ordered(tools, sort);
    let panel = InsightsPanel::Tools;
    let lines = panel.room().expect("the Tools panel scrolls");
    let window = RowWindow::new(ordered.len(), lines, view.scrolled(panel));
    let rows = ordered
        .iter()
        .enumerate()
        .skip(window.first)
        .take(window.shown);
    for (y, (place, tool)) in (area.y + 1..).zip(rows) {
        let text = line(
            &label(&tool.tool),
            &grouped_count(tool.uses),
            &grouped_count(tool.failed),
        );
        let marked = selected == Some(place);
        let style = if marked {
            theme.accent.primary.add_modifier(Modifier::REVERSED)
        } else {
            theme.text.primary
        };
        buffer.set_string(area.x, y, truncate_to_width(&text, room), style);
        // Failures stand out in the error colour, where the column fits.
        if tool.failed > 0 && !marked && label_width > 0 {
            let failed = format!("{:>FAILED_WIDTH$}", grouped_count(tool.failed));
            let x = area.right() - u16::try_from(FAILED_WIDTH).unwrap_or(0);
            buffer.set_string(x, y, failed, theme.feedback.error);
        }
    }
    if window.folded > 0 {
        let y = area.y + 1 + u16::try_from(window.shown).unwrap_or(u16::MAX);
        let folded = u64::try_from(window.folded).unwrap_or(u64::MAX);
        let fold = format!("({} more)", grouped_count(folded));
        buffer.set_string(area.x, y, truncate_to_width(&fold, room), subdued);
    }
    let shown = u16::try_from(window.shown).unwrap_or(u16::MAX);
    view.record_item_targets(
        panel,
        Rect::new(area.x, area.y + 1, area.width, shown),
        area.width,
        window.first,
    );
}
