//! The Spinner: the animated glyph a client shows where work is live right
//! now — the Marker of an Active Activity and the footer's active state.
//!
//! Animation never reaches the memoized transcript projection (ADR 0009): the
//! projection renders every live Marker as [`MARKER`], the Spinner's first
//! frame, and records which lines carry one; each draw patches the current
//! frame's glyph into those cells after the memoized lines are fetched. The
//! run loop drives the frame index with a tick that exists only while
//! something on screen is animating, so an idle TUI schedules zero wakeups.

use ratatui::text::Line;

/// The Spinner's frames, each one cell wide so a Marker's outcome glyph can
/// land in the same cell once the Activity settles.
pub(super) const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The Marker a projection renders for an Active Activity: the Spinner's
/// first frame, in the marker idiom the settled glyphs (`"✓ "`, `"× "`) use.
pub(super) const MARKER: &str = "⠋ ";

/// The glyph the overlay looks for when patching a recorded line. Always the
/// first frame, because that is what the projection rendered.
const PLACEHOLDER: char = '⠋';

/// How long each frame holds before the run loop's tick advances it.
pub(super) const TICK_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

/// The frame glyph for a run-loop frame index, cycling forever.
pub(super) fn frame(index: usize) -> &'static str {
    FRAMES[index % FRAMES.len()]
}

/// Patches the current frame into the Marker cell of each recorded line. The
/// projection rendered the placeholder there, so the first span containing it
/// holds the Marker — any later occurrence is content and stays untouched.
pub(super) fn overlay_frame(lines: &mut [Line<'static>], spinner_lines: &[usize], index: usize) {
    let glyph = frame(index);
    if glyph == FRAMES[0] {
        return;
    }
    for &line_index in spinner_lines {
        let Some(line) = lines.get_mut(line_index) else {
            continue;
        };
        for span in &mut line.spans {
            if let Some(position) = span.content.find(PLACEHOLDER) {
                let mut content = span.content.to_string();
                content.replace_range(position..position + PLACEHOLDER.len_utf8(), glyph);
                span.content = content.into();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn marker_is_the_first_frame() {
        assert_eq!(MARKER, format!("{} ", FRAMES[0]));
        assert_eq!(PLACEHOLDER.to_string(), FRAMES[0]);
    }

    #[test]
    fn frames_are_one_cell_wide() {
        use unicode_width::UnicodeWidthStr;
        for frame in FRAMES {
            assert_eq!(frame.width(), 1, "frame {frame} must fill exactly one cell");
        }
    }

    #[test]
    fn overlay_patches_only_recorded_lines() {
        let mut lines = vec![
            Line::from(format!("  {MARKER}cargo build")),
            Line::from(format!("  {MARKER}not recorded")),
        ];
        overlay_frame(&mut lines, &[0], 1);
        assert_eq!(
            lines[0].spans[0].content,
            format!("  {} cargo build", FRAMES[1])
        );
        assert_eq!(lines[1].spans[0].content, format!("  {MARKER}not recorded"));
    }

    #[test]
    fn overlay_patches_the_marker_not_the_content() {
        let mut lines = vec![Line::from(format!("  {MARKER}echo {PLACEHOLDER}"))];
        overlay_frame(&mut lines, &[0], 2);
        assert_eq!(
            lines[0].spans[0].content,
            format!("  {} echo {PLACEHOLDER}", FRAMES[2])
        );
    }

    #[test]
    fn overlay_leaves_lines_alone_on_the_first_frame() {
        let mut lines = vec![Line::from(format!("  {MARKER}cargo build"))];
        overlay_frame(&mut lines, &[0], FRAMES.len());
        assert_eq!(lines[0].spans[0].content, format!("  {MARKER}cargo build"));
    }

    #[test]
    fn overlay_finds_the_marker_behind_an_indent_span() {
        let mut lines = vec![Line::from(vec![
            Span::raw("  "),
            Span::raw(format!("  {MARKER}Thinking")),
        ])];
        overlay_frame(&mut lines, &[0], 3);
        assert_eq!(
            lines[0].spans[1].content,
            format!("  {} Thinking", FRAMES[3])
        );
    }
}
