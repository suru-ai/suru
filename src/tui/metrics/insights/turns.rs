//! The Turns panel: how long the range's Turns took to Settle — the median,
//! the p90, and the longest, then the spread across fixed bins as a
//! sparkline whose bins a reader can select — and how many Completed,
//! Failed, and were Interrupted.

use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};

use crate::{
    protocol::{TURN_DURATION_BINS, TurnDurations, TurnInsights},
    theme::Theme,
};

use super::{
    super::{
        super::{slots::truncate_to_width, transcript::humanized_duration, usage::grouped_count},
        calendar::MetricsRange,
        view::share,
    },
    InsightsPanel, InsightsView,
    view::render_spans,
};

/// The rows the panel draws inside its border.
pub(super) const ROWS: u16 = 3;

/// How each bin is named, in their order.
const BIN_LABELS: [&str; TURN_DURATION_BINS] = [
    "<10s", "10–30s", "30s–1m", "1–2m", "2–5m", "5–15m", "15–30m", "30m+",
];

/// The eighth-block glyphs a bin's height is drawn in, the lowest first.
const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// How many items the panel offers for selection: a bin apiece where the
/// Turns recorded durations, and none where they did not.
pub(super) fn items(turns: &TurnInsights) -> usize {
    if turns.durations.is_some() {
        TURN_DURATION_BINS
    } else {
        0
    }
}

/// Draws the panel's figures in `area`, inside its border, marking the bin
/// `selected` and naming its figures in place of the bins' span.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    turns: &TurnInsights,
    range: MetricsRange,
    selected: Option<usize>,
    view: &InsightsView,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;
    if turns.completed + turns.failed + turns.interrupted == 0 {
        let note = format!("No Turns {}", range.within());
        buffer.set_string(area.x, area.y, truncate_to_width(&note, room), subdued);
        return;
    }
    match &turns.durations {
        Some(durations) => {
            let stated = format!(
                "median {} · p90 {} · longest {}",
                humanized_duration(durations.median_ms),
                humanized_duration(durations.p90_ms),
                humanized_duration(durations.longest_ms),
            );
            buffer.set_string(area.x, area.y, truncate_to_width(&stated, room), primary);
            render_bins(buffer, area, durations, selected, view, theme);
        }
        None => {
            let note = "No Turn recorded how long it took";
            buffer.set_string(area.x, area.y, truncate_to_width(note, room), subdued);
        }
    }
    let ended = [
        ("✓", turns.completed, "completed", theme.feedback.success),
        ("✗", turns.failed, "failed", theme.feedback.error),
        (
            "⊘",
            turns.interrupted,
            "interrupted",
            theme.feedback.warning,
        ),
    ];
    let spans = ended
        .into_iter()
        .enumerate()
        .flat_map(|(place, (glyph, count, name, style))| {
            let gap = if place == 0 { "" } else { "  " };
            [
                (format!("{gap}{glyph} "), style),
                (format!("{} {name}", grouped_count(count)), primary),
            ]
        });
    render_spans(buffer, area, area.y + 2, spans);
}

/// `count` Turns, written whole.
pub(super) fn counted(count: u64) -> String {
    match count {
        1 => "1 Turn".to_owned(),
        count => format!("{} Turns", grouped_count(count)),
    }
}

/// The sparkline of the bins, each as high as its count is of the fullest
/// bin's, and beside it the bins' span or the selected bin's figures.
fn render_bins(
    buffer: &mut Buffer,
    area: Rect,
    durations: &TurnDurations,
    selected: Option<usize>,
    view: &InsightsView,
    theme: &Theme,
) {
    let y = area.y + 1;
    let fullest = durations.bins.iter().copied().max().unwrap_or(0);
    let bins = u16::try_from(TURN_DURATION_BINS).unwrap_or(u16::MAX);
    if area.width < bins {
        return;
    }
    for (bin, count) in durations.bins.iter().copied().enumerate() {
        // Any bin holding a Turn stands at least an eighth high.
        let glyph = match (u128::from(count) * 8).div_ceil(u128::from(fullest.max(1))) {
            0 => " ",
            level => LEVELS[usize::try_from(level).unwrap_or(8).min(8) - 1],
        };
        let style = if selected == Some(bin) {
            theme.accent.primary.add_modifier(Modifier::REVERSED)
        } else {
            theme.accent.primary
        };
        let x = area.x + u16::try_from(bin).unwrap_or(0);
        buffer.set_string(x, y, glyph, style);
    }
    view.record_item_targets(InsightsPanel::Turns, Rect::new(area.x, y, bins, 1), 1, 0);
    let timed: u64 = durations.bins.iter().sum();
    let caption = match selected {
        Some(bin) => format!(
            "{} · {} · {}",
            BIN_LABELS[bin],
            counted(durations.bins[bin]),
            share(durations.bins[bin], timed),
        ),
        None => format!("{} … {}", BIN_LABELS[0], BIN_LABELS[TURN_DURATION_BINS - 1]),
    };
    let x = area.x + bins + 2;
    let room = usize::from(area.right().saturating_sub(x));
    let style = if selected.is_some() {
        theme.text.primary
    } else {
        theme.text.subdued
    };
    buffer.set_string(x, y, truncate_to_width(&caption, room), style);
}
