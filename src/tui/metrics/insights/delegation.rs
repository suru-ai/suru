//! The Delegation panel: how the range's Cost divides between the user's
//! own Sessions, their Subagents — the brokered share of those beside them
//! — Sidekicks, and Subsessions, each role's share of the whole with a bar
//! and its Cost, `+` where that Cost is partial — and on every share of
//! work where the whole is, since a share of a partial whole is no more
//! certain than its amount. Beneath them, a note that a
//! Claude Session's native Subagents are inside its own Cost, since Claude
//! reports a whole tree's Cost on the Session it runs them in.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    protocol::{Cost, DelegationInsights, RoleCost},
    theme::Theme,
};

use super::{
    super::{super::slots::truncate_to_width, calendar::MetricsRange, view::share},
    view::{bar, render_spans, stated_cost},
};

/// The rows the panel draws inside its border: a role apiece, then the
/// note.
pub(super) const ROWS: u16 = 5;

/// How wide the roles' names stand, wide enough for the longest.
const LABEL_WIDTH: usize = 12;

/// How wide a share stands, wide enough for "100%+".
const SHARE_WIDTH: usize = 5;

/// How wide a Cost stands, wide enough for thousands of dollars marked
/// partial.
const COST_WIDTH: usize = 10;

/// What stands before a role's bar: its name, share, and Cost, and the gaps
/// between them.
const LEAD_WIDTH: usize = LABEL_WIDTH + 1 + SHARE_WIDTH + 2 + COST_WIDTH + 2;

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
            share_of(cost, total),
            stated(cost),
        );
        // The brokered share stands beside its Subagents wherever any of
        // their work was brokered.
        let brokered = brokered
            .filter(|brokered| brokered.cost.is_some() || brokered.is_cost_partial)
            .map(|brokered| format!("  brokered {}", share_of(brokered, total)));
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

/// `cost`'s share of `whole`, `+` where the whole is partial and `cost`
/// counts work at all: a role that did none has no share however much of
/// the whole went unpriced.
fn share_of(cost: RoleCost, whole: RoleCost) -> String {
    let stated = share(nano_usd(cost), nano_usd(whole));
    let did_work = cost.cost.is_some() || cost.is_cost_partial;
    if whole.is_cost_partial && did_work {
        stated + "+"
    } else {
        stated
    }
}

/// What `cost` counts, in nano-dollars: nothing where it counts no amount.
fn nano_usd(cost: RoleCost) -> u64 {
    cost.cost.map_or(0, Cost::nano_usd)
}

/// `cost` as the panel states it: see [`stated_cost`].
fn stated(cost: RoleCost) -> String {
    stated_cost(cost.cost, cost.is_cost_partial)
}
