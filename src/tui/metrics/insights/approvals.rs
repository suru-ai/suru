//! The Approvals panel: how many the range's Turns asked, and how many of
//! those were accepted, declined, or are still waiting or expired with no
//! Decision recorded — beneath that, how many were accepted for the whole
//! Session, and how many declined also interrupted their Turn, on a line
//! apiece where the column is too narrow for both on one. Where it is
//! narrow, how many were decided each way stand by their glyphs alone.

use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::ApprovalInsights, theme::Theme};

use super::{
    super::{
        super::{slots::truncate_to_width, usage::grouped_count},
        calendar::MetricsRange,
    },
    view::{fitting, render_fitting_spans},
};

/// The rows the panel draws inside its border.
pub(super) const ROWS: u16 = 4;

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
        let note = fitting(
            room,
            [
                format!("No Approvals asked {}", range.within()),
                "No Approvals asked".to_owned(),
            ],
        );
        buffer.set_string(area.x, area.y, note, subdued);
        return;
    }
    let asked = format!("{} asked", grouped_count(approvals.asked));
    buffer.set_string(area.x, area.y, truncate_to_width(&asked, room), primary);
    let accepted = approvals.accepted + approvals.accepted_for_session;
    let declined = approvals.declined + approvals.declined_and_interrupted;
    let undecided = approvals.waiting + approvals.expired;
    let decided = |named: bool| {
        [
            ("✓", accepted, "accepted", theme.feedback.success),
            ("✗", declined, "declined", theme.feedback.error),
            ("⧗", undecided, "waiting or expired", theme.feedback.warning),
        ]
        .into_iter()
        .enumerate()
        .flat_map(|(place, (glyph, count, name, style))| {
            let gap = if place == 0 { "" } else { "  " };
            let count = grouped_count(count);
            let figure = if named {
                format!("{count} {name}")
            } else {
                count
            };
            [(format!("{gap}{glyph} "), style), (figure, primary)]
        })
        .collect()
    };
    render_fitting_spans(buffer, area, area.y + 1, [decided(true), decided(false)]);
    let for_session = format!(
        "{} for the Session",
        grouped_count(approvals.accepted_for_session)
    );
    let interrupted = grouped_count(approvals.declined_and_interrupted);
    let together = format!("{for_session} · {interrupted} also interrupted the Turn");
    if together.width() <= room {
        buffer.set_string(area.x, area.y + 2, together, subdued);
        return;
    }
    let interrupted = fitting(
        room,
        [
            format!("{interrupted} also interrupted the Turn"),
            format!("{interrupted} interrupted the Turn"),
        ],
    );
    buffer.set_string(area.x, area.y + 2, fitting(room, [for_session]), subdued);
    buffer.set_string(area.x, area.y + 3, interrupted, subdued);
}
