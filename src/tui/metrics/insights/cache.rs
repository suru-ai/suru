//! The Cache panel: the share of the range's input read from cache, and an
//! estimate of what caching saved at today's rates — labelled as one, and
//! never a Cost.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    protocol::{CacheInsights, EstimatedSaving},
    theme::Theme,
};

use super::super::{
    super::{slots::truncate_to_width, usage::compact_dollars},
    calendar::MetricsRange,
};

/// The rows the panel draws inside its border.
pub(super) const ROWS: u16 = 2;

/// Draws the panel's figures in `area`, inside its border.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    cache: &CacheInsights,
    range: MetricsRange,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;
    let Some(share) = cache.read_share() else {
        let note = format!("No input {}", range.within());
        buffer.set_string(area.x, area.y, truncate_to_width(&note, room), subdued);
        return;
    };
    // Rounded to the whole percent as the headline rounds it.
    let share = format!("{}% of input from cache", (share + 50) / 100);
    buffer.set_string(area.x, area.y, truncate_to_width(&share, room), primary);
    let (saving, style) = match saved(cache.saving) {
        Some(saved) => (saved, primary),
        None if cache.saving.is_partial => ("No rates to estimate a saving at".to_owned(), subdued),
        None => ("Nothing read from or written to cache".to_owned(), subdued),
    };
    buffer.set_string(area.x, area.y + 1, truncate_to_width(&saving, room), style);
}

/// What `saving` says caching saved, a `+` after it where a Model's Turns
/// are left out, and a minus before it where writing the cache cost more
/// than reading it saved; none where no Model's rates could estimate it.
fn saved(saving: EstimatedSaving) -> Option<String> {
    let nano_usd = saving.nano_usd?;
    let amount = compact_dollars(nano_usd.unsigned_abs());
    let sign = if nano_usd < 0 { "−" } else { "" };
    let partial = if saving.is_partial { "+" } else { "" };
    Some(format!("saved ≈ {sign}{amount}{partial} (est.)"))
}
