//! The Compactions panel: how many the range's Turns held, how many of
//! those the user asked for, how many failed, and the share of the context
//! they freed on average, where any could say.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    protocol::{CompactionInsights, FreedShare},
    theme::Theme,
};

use super::{
    super::{
        super::{slots::truncate_to_width, usage::grouped_count},
        calendar::MetricsRange,
    },
    view::render_spans,
};

/// The rows the panel draws inside its border.
pub(super) const ROWS: u16 = 2;

/// Draws the panel's figures in `area`, inside its border.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    compactions: &CompactionInsights,
    range: MetricsRange,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;
    let total = compactions.total();
    if total == 0 {
        let note = format!("No Compactions {}", range.within());
        buffer.set_string(area.x, area.y, truncate_to_width(&note, room), subdued);
        return;
    }
    let counted = match total {
        1 => "1 Compaction".to_owned(),
        total => format!("{} Compactions", grouped_count(total)),
    };
    let failed = compactions.automatic.failed + compactions.manual.failed;
    render_spans(
        buffer,
        area,
        area.y,
        [
            (
                format!(
                    "{counted} · {} manual · ",
                    grouped_count(compactions.manual.total())
                ),
                primary,
            ),
            ("✗ ".to_owned(), theme.feedback.error),
            (format!("{} failed", grouped_count(failed)), primary),
        ],
    );
    let (freed, style) = match compactions.freed {
        Some(freed) => (freed_on_average(freed), primary),
        None => ("No Compaction recorded what it freed".to_owned(), subdued),
    };
    buffer.set_string(area.x, area.y + 1, truncate_to_width(&freed, room), style);
}

/// What `freed` says of the context a Compaction leaves, to the nearest
/// whole percent of what it found.
fn freed_on_average(freed: FreedShare) -> String {
    let percent = (freed.average_millionths.unsigned_abs() + 5_000) / 10_000;
    if freed.average_millionths < 0 {
        format!("leaves {percent}% more context on average")
    } else {
        format!("frees {percent}% of context on average")
    }
}
