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
    view::{fitting, nothing, render_fitting_spans, tallies},
};

/// The most rows the panel draws inside its border: how many were asked,
/// how they were decided, and how many were accepted for the Session and
/// how many declined also interrupted their Turn, on one line or two.
const ROWS: u16 = 4;

/// The rows the panel draws inside its border, `room` columns wide: a row
/// fewer where those accepted for the Session and those that interrupted
/// their Turn stand on one line, or none were asked.
pub(super) fn rows(approvals: &ApprovalInsights, room: usize) -> u16 {
    if approvals.asked == 0 || together(approvals).width() <= room {
        ROWS - 1
    } else {
        ROWS
    }
}

/// How many were accepted for the Session and how many declined also
/// interrupted their Turn, on one line.
fn together(approvals: &ApprovalInsights) -> String {
    format!(
        "{} for the Session · {} also interrupted the Turn",
        grouped_count(approvals.accepted_for_session),
        grouped_count(approvals.declined_and_interrupted),
    )
}

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
        let note = nothing(room, "No Approvals asked", range);
        buffer.set_string(area.x, area.y, note, subdued);
        return;
    }
    let asked = format!("{} asked", grouped_count(approvals.asked));
    buffer.set_string(area.x, area.y, truncate_to_width(&asked, room), primary);
    let accepted = approvals.accepted + approvals.accepted_for_session;
    let declined = approvals.declined + approvals.declined_and_interrupted;
    let undecided = approvals.waiting + approvals.expired;
    let decided = [
        ("✓", accepted, "accepted", theme.feedback.success),
        ("✗", declined, "declined", theme.feedback.error),
        ("⧗", undecided, "waiting or expired", theme.feedback.warning),
    ];
    render_fitting_spans(
        buffer,
        area,
        area.y + 1,
        [
            tallies(&decided, true, primary),
            tallies(&decided, false, primary),
        ],
    );
    let together = together(approvals);
    if together.width() <= room {
        buffer.set_string(area.x, area.y + 2, together, subdued);
        return;
    }
    let for_session = format!(
        "{} for the Session",
        grouped_count(approvals.accepted_for_session)
    );
    let interrupted = grouped_count(approvals.declined_and_interrupted);
    let interrupted = fitting(
        room,
        [
            format!("{interrupted} also interrupted the Turn"),
            format!("{interrupted} interrupted the Turn"),
        ],
    );
    let for_session = truncate_to_width(&for_session, room);
    buffer.set_string(area.x, area.y + 2, for_session, subdued);
    buffer.set_string(area.x, area.y + 3, interrupted, subdued);
}
