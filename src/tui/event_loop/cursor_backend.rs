use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

use super::TerminalSink;

/// Whether the terminal was last told to show or hide its cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Unknown,
    Hidden,
    Shown,
}

/// A backend that only passes cursor traffic on when it would change something.
///
/// ratatui re-emits Show and MoveTo on every draw, and the run loop redraws
/// every 32 ms while anything animates. Terminals restart the cursor blink
/// phase on both sequences, so the composer caret is put back in its lit phase
/// far more often than any blink interval and never goes dark. Remembering what
/// was last written lets an unchanged caret cost nothing at all.
///
/// A frame that repaints anything hides the cursor ahead of its diff. That is
/// what makes the synchronized-output fix safe on terminals that ignore mode
/// 2026 -- legacy conhost among them -- where the diff is otherwise presented
/// cell by cell with the caret riding along. The known cost: on such a
/// terminal, one that also restarts the blink phase on Show, the caret looks
/// solid while a spinner animates, since every repainting frame hides and
/// re-shows it. Idle redraws (empty diff, unchanged caret) still write nothing
/// and keep blinking.
pub(super) struct QuietCursorBackend<B> {
    inner: B,
    visibility: Visibility,
    /// The last MoveTo written, or `None` when the terminal's cursor is
    /// somewhere else: unknown at start, or displaced by the diff's own moves.
    position: Option<Position>,
}

impl<B> QuietCursorBackend<B> {
    pub(super) fn new(inner: B) -> Self {
        Self {
            inner,
            visibility: Visibility::Unknown,
            position: None,
        }
    }

    /// Forgets what the terminal was last told, so the next frame re-asserts
    /// both visibility and position. Called whenever the display is
    /// (re)entered: entering writes its own Hide through the sink, past this
    /// memory, and a resumed terminal may have been left with any cursor.
    pub(super) fn forget_cursor(&mut self) {
        self.visibility = Visibility::Unknown;
        self.position = None;
    }

    #[cfg(test)]
    pub(super) fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: Backend> Backend for QuietCursorBackend<B> {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_some() {
            // A cursor that might be shown is dragged across every repainted
            // cell on a terminal that presents the diff as it arrives, so it
            // is hidden ahead of the diff. ratatui shows and parks it again
            // afterwards, and the memory makes that a single Show.
            self.hide_cursor()?;
            // The diff's own moves leave the terminal cursor on the last
            // repainted cell, so an unchanged caret still has to be parked
            // again afterwards.
            self.position = None;
        }
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        if self.visibility != Visibility::Hidden {
            self.inner.hide_cursor()?;
            self.visibility = Visibility::Hidden;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        if self.visibility != Visibility::Shown {
            self.inner.show_cursor()?;
            self.visibility = Visibility::Shown;
        }
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        let position = position.into();
        if self.position != Some(position) {
            self.inner.set_cursor_position(position)?;
            self.position = Some(position);
        }
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.position = None;
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.position = None;
        self.inner.append_lines(n)
    }

    fn size(&self) -> std::io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Raw writes go straight through: the colour probe and the clipboard
/// replies are written on the backend, not drawn through it.
impl<B: std::io::Write> std::io::Write for QuietCursorBackend<B> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(buf)
    }
}

/// Mode changes bypass the cursor memory by design: the Hide written on
/// display entry is followed by [`QuietCursorBackend::forget_cursor`], which
/// is what keeps the memory honest.
impl<B: TerminalSink> TerminalSink for QuietCursorBackend<B> {
    fn apply(&mut self, command: impl crossterm::Command) -> std::io::Result<()> {
        self.inner.apply(command)
    }
}
