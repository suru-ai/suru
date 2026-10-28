//! The Compactions panel: how many the range's Turns held, how many of
//! those the user asked for, how many failed, and the share of the context
//! they freed on average, where any could say — more briefly where its
//! column is narrow.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    protocol::{CompactionInsights, FreedShare},
    theme::Theme,
};

use super::{
    super::{super::usage::grouped_count, calendar::MetricsRange},
    view::{fitting, nothing, render_fitting_spans},
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
        let note = nothing(room, "No Compactions", range);
        buffer.set_string(area.x, area.y, note, subdued);
        return;
    }
    let counted = match total {
        1 => "1 Compaction".to_owned(),
        total => format!("{} Compactions", grouped_count(total)),
    };
    let failed = grouped_count(compactions.automatic.failed + compactions.manual.failed);
    let manual = grouped_count(compactions.manual.total());
    let form = |counted: &str, failed: String| {
        vec![
            (format!("{counted} · {manual} manual · "), primary),
            ("✗ ".to_owned(), theme.feedback.error),
            (failed, primary),
        ]
    };
    let total = grouped_count(total);
    render_fitting_spans(
        buffer,
        area,
        area.y,
        [
            form(&counted, format!("{failed} failed")),
            form(&total, format!("{failed} failed")),
            form(&total, failed),
        ],
    );
    let (freed, style) = match compactions.freed {
        Some(freed) => (fitting(room, freed_on_average(freed)), primary),
        None => (
            fitting(
                room,
                [
                    "No Compaction recorded what it freed".to_owned(),
                    "No record of what it freed".to_owned(),
                ],
            ),
            subdued,
        ),
    };
    buffer.set_string(area.x, area.y + 1, freed, style);
}

/// What `freed` says of the context a Compaction leaves, to the nearest
/// whole percent of what it found, fully and briefly: an average that
/// rounds to none frees nothing, whichever side of none it lies.
fn freed_on_average(freed: FreedShare) -> [String; 2] {
    let percent = (freed.average_millionths.unsigned_abs() + 5_000) / 10_000;
    if freed.average_millionths < 0 && percent > 0 {
        [
            format!("leaves {percent}% more context on average"),
            format!("leaves {percent}% more on average"),
        ]
    } else {
        [
            format!("frees {percent}% of context on average"),
            format!("frees {percent}% on average"),
        ]
    }
}
