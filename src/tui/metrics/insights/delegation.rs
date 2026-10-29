//! The Delegation panel: how the range's Cost divides between the user's
//! own Sessions, their Subagents — the brokered share of those beside them
//! — Sidekicks, and Subsessions, each role's share of the whole with a bar
//! and its Cost, `+` where that Cost is partial — and on every share of
//! work where the whole is, since a share of a partial whole is no more
//! certain than its amount. Beneath them, a note that a
//! Claude Session's native Subagents are inside its own Cost, since Claude
//! reports a whole tree's Cost on the Session it runs them in.

use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{Cost, CountedCost, DelegationInsights},
    theme::Theme,
};

use super::{
    super::{calendar::MetricsRange, view::share},
    view::{bar, fitting, nothing, render_spans, stated_cost},
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

/// What stands before a role's bar where its Cost is left out.
const BRIEF_LEAD_WIDTH: usize = LABEL_WIDTH + 1 + SHARE_WIDTH + 2;

/// The note beneath the roles, fully and briefly.
const NOTE: [&str; 2] = [
    "A Claude Session's native Subagents are in its Cost",
    "A Claude Session's Subagents are in its Cost",
];

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
    let Some(whole) = total.amount.map(Cost::nano_usd) else {
        let note = if total.is_partial {
            "No work priced"
        } else {
            "No Cost"
        };
        let note = nothing(room, note, range);
        buffer.set_string(area.x, area.y, note, subdued);
        return;
    };
    let subagents = [delegation.native_subagents, delegation.brokered_subagents]
        .into_iter()
        .sum();
    let roles = [
        ("own Sessions", delegation.own_sessions, None),
        ("Subagents", subagents, Some(delegation.brokered_subagents)),
        ("Sidekicks", delegation.sidekicks, None),
        ("Subsessions", delegation.subsessions, None),
    ];
    // The brokered share stands beside its Subagents wherever any of their
    // work was brokered.
    let roles = roles.map(|(label, cost, brokered)| {
        let brokered = brokered
            .filter(|brokered| brokered.counts_work())
            .map(|brokered| format!("  brokered {}", share_of(brokered, total)))
            .unwrap_or_default();
        (label, cost, brokered)
    });
    // Each role's Cost stands beside its share where every bar and the
    // brokered share after its own still fit, and is left out where they
    // would not: the share says how much, and the bars and the brokered
    // share are the panel's point.
    let fits = |lead: usize| {
        let bar_width = room.saturating_sub(lead);
        roles.iter().all(|(_, cost, brokered)| {
            bar(nano_usd(*cost), whole, bar_width).width() + brokered.width() <= bar_width
        })
    };
    let costed = fits(LEAD_WIDTH);
    let lead_width = if costed { LEAD_WIDTH } else { BRIEF_LEAD_WIDTH };
    let bar_width = room.saturating_sub(lead_width);
    for (y, (label, cost, brokered)) in (area.y..).zip(roles) {
        let share = share_of(cost, total);
        let lead = if costed {
            format!(
                "{label:<LABEL_WIDTH$} {share:>SHARE_WIDTH$}  {:>COST_WIDTH$}  ",
                stated_cost(cost),
            )
        } else {
            format!("{label:<LABEL_WIDTH$} {share:>SHARE_WIDTH$}  ")
        };
        render_spans(
            buffer,
            area,
            y,
            [
                (lead, primary),
                (bar(nano_usd(cost), whole, bar_width), theme.accent.primary),
                (brokered, subdued),
            ],
        );
    }
    let note = fitting(room, NOTE.map(str::to_owned));
    buffer.set_string(area.x, area.y + 4, note, subdued);
}

/// `cost`'s share of `whole`, `+` where the whole is partial and `cost`
/// counts work at all: a role that did none has no share however much of
/// the whole went unpriced.
fn share_of(cost: CountedCost, whole: CountedCost) -> String {
    let stated = share(nano_usd(cost), nano_usd(whole));
    if whole.is_partial && cost.counts_work() {
        stated + "+"
    } else {
        stated
    }
}

/// What `cost` counts, in nano-dollars: nothing where it counts no amount.
fn nano_usd(cost: CountedCost) -> u64 {
    cost.amount.map_or(0, Cost::nano_usd)
}
