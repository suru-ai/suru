//! The Delegation panel: how the range's Cost divides between the user's
//! own Sessions, their Subagents — the brokered share of those beside them
//! — Sidekicks, and Subsessions, each role's share of the whole with a bar
//! and its Cost, `+` where that Cost is partial. Beneath them, a note that a
//! Claude Session's native Subagents are inside its own Cost, since Claude
//! reports a whole tree's Cost on the Session it runs them in.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    protocol::{Cost, DelegationInsights, RoleCost},
    theme::Theme,
};

use super::{
    super::{
        super::slots::truncate_to_width, calendar::MetricsRange, chart::MetricsMeasure, view::share,
    },
    view::render_spans,
};

/// The rows the panel draws inside its border: a role apiece, then the
/// note.
pub(super) const ROWS: u16 = 5;

/// How wide the roles' names stand, wide enough for the longest.
const LABEL_WIDTH: usize = 12;

/// How wide a share stands, wide enough for "100%".
const SHARE_WIDTH: usize = 4;

/// How wide a Cost stands, wide enough for thousands of dollars marked
/// partial.
const COST_WIDTH: usize = 10;

/// What stands before a role's bar: its name, share, and Cost, and the gaps
/// between them.
const LEAD_WIDTH: usize = LABEL_WIDTH + 1 + SHARE_WIDTH + 2 + COST_WIDTH + 2;

/// The glyphs a bar's last cell is drawn in, an eighth of a cell full to
/// seven eighths.
const EIGHTHS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

const NOTE: &str = "A Claude Session's native Subagents are in its Cost";

/// Draws the panel's figures in `area`, inside its border.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    delegation: &DelegationInsights,
    range: MetricsRange,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;
    let total = delegation.total();
    let Some(whole) = total.cost.map(Cost::nano_usd) else {
        let note = if total.is_cost_partial {
            format!("No work priced {}", range.within())
        } else {
            format!("No Cost {}", range.within())
        };
        buffer.set_string(area.x, area.y, truncate_to_width(&note, room), subdued);
        return;
    };
    let subagents = delegation
        .native_subagents
        .with(delegation.brokered_subagents);
    let roles = [
        ("own Sessions", delegation.own_sessions, None),
        ("Subagents", subagents, Some(delegation.brokered_subagents)),
        ("Sidekicks", delegation.sidekicks, None),
        ("Subsessions", delegation.subsessions, None),
    ];
    let bar_width = room.saturating_sub(LEAD_WIDTH);
    for (y, (label, cost, brokered)) in (area.y..).zip(roles) {
        let part = nano_usd(cost);
        let lead = format!(
            "{label:<LABEL_WIDTH$} {:>SHARE_WIDTH$}  {:>COST_WIDTH$}  ",
            share(part, whole),
            stated(cost),
        );
        // The brokered share stands beside its Subagents wherever any of
        // their work was brokered.
        let brokered = brokered
            .filter(|brokered| brokered.cost.is_some() || brokered.is_cost_partial)
            .map(|brokered| format!("  brokered {}", share(nano_usd(brokered), whole)));
        render_spans(
            buffer,
            area,
            y,
            [
                (lead, primary),
                (bar(part, whole, bar_width), theme.accent.primary),
                (brokered.unwrap_or_default(), subdued),
            ],
        );
    }
    buffer.set_string(area.x, area.y + 4, truncate_to_width(NOTE, room), subdued);
}

/// What `cost` counts, in nano-dollars: nothing where it counts no amount.
fn nano_usd(cost: RoleCost) -> u64 {
    cost.cost.map_or(0, Cost::nano_usd)
}

/// `cost` as the panel states it: its amount, `+` where it is partial; or,
/// where it counts no amount, whether that is for want of work or of a
/// price, never a zero nobody measured.
fn stated(cost: RoleCost) -> String {
    match cost {
        RoleCost {
            cost: Some(cost),
            is_cost_partial,
        } => MetricsMeasure::Cost.figure(cost.nano_usd(), is_cost_partial),
        RoleCost {
            cost: None,
            is_cost_partial: true,
        } => "unpriced".to_owned(),
        RoleCost {
            cost: None,
            is_cost_partial: false,
        } => "—".to_owned(),
    }
}

/// A bar as long, in eighths of `width` cells, as `part` is of `whole`, to
/// the nearest eighth: any part at all at least an eighth long.
fn bar(part: u64, whole: u64, width: usize) -> String {
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
