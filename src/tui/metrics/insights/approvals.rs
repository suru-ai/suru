//! The Approvals panel: how many the range's Turns asked, and how many of
//! those were accepted, declined, or are still waiting or expired with no
//! Decision recorded — beneath that, how many were accepted for the whole
//! Session, and how many declined also interrupted their Turn.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::{protocol::ApprovalInsights, theme::Theme};

use super::{
    super::{
        super::{slots::truncate_to_width, usage::grouped_count},
        calendar::MetricsRange,
    },
    view::render_spans,
};

/// The rows the panel draws inside its border.
pub(super) const ROWS: u16 = 3;

/// Draws the panel's figures in `area`, inside its border.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    approvals: &ApprovalInsights,
    range: MetricsRange,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;
    if approvals.asked == 0 {
        let note = format!("No Approvals asked {}", range.within());
        buffer.set_string(area.x, area.y, truncate_to_width(&note, room), subdued);
        return;
    }
    let asked = format!("{} asked", grouped_count(approvals.asked));
    buffer.set_string(area.x, area.y, truncate_to_width(&asked, room), primary);
    let accepted = approvals.accepted + approvals.accepted_for_session;
    let declined = approvals.declined + approvals.declined_and_interrupted;
    let undecided = approvals.waiting + approvals.expired;
    render_spans(
        buffer,
        area,
        area.y + 1,
        [
            ("✓ ".to_owned(), theme.feedback.success),
            (format!("{} accepted", grouped_count(accepted)), primary),
            ("  ✗ ".to_owned(), theme.feedback.error),
            (format!("{} declined", grouped_count(declined)), primary),
            ("  ⧗ ".to_owned(), theme.feedback.warning),
            (
                format!("{} waiting or expired", grouped_count(undecided)),
                primary,
            ),
        ],
    );
    let apart = format!(
        "{} for the Session · {} also interrupted the Turn",
        grouped_count(approvals.accepted_for_session),
        grouped_count(approvals.declined_and_interrupted),
    );
    buffer.set_string(area.x, area.y + 2, truncate_to_width(&apart, room), subdued);
}
