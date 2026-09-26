use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

use crossterm::{
    Command,
    cursor::{Hide, MoveTo, Show},
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Buffer,
    buffer::Cell,
    layout::{Position, Size},
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
struct HyperlinkCell {
    target: String,
    cell: Cell,
}

type Hyperlinks = BTreeMap<(u16, u16), HyperlinkCell>;

#[derive(Default)]
struct HyperlinkFrame {
    enabled: bool,
    targets: BTreeMap<(u16, u16), String>,
    cells: Hyperlinks,
    covered: BTreeSet<(u16, u16)>,
}

thread_local! {
    static HYPERLINK_FRAME: RefCell<HyperlinkFrame> = RefCell::new(HyperlinkFrame::default());
}

pub(super) fn begin_frame(enabled: bool) {
    HYPERLINK_FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.enabled = enabled;
        frame.targets.clear();
        frame.cells.clear();
        frame.covered.clear();
    });
}

pub(crate) fn register_hyperlink(buffer: &Buffer, x: u16, y: u16, width: u16, target: &str) {
    HYPERLINK_FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        if !frame.enabled || !buffer.area.contains(Position::new(x, y)) {
            return;
        }
        let right = x.saturating_add(width).min(buffer.area.right());
        let mut column = x;
        while column < right {
            frame.targets.insert((column, y), target.to_owned());
            let symbol_width = buffer[(column, y)].symbol().width().max(1);
            column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX));
        }
    });
}

/// Captures the final cells after overlays and selection highlights have drawn,
/// including the columns covered by wide graphemes. Those continuation cells
/// must never be repainted independently during an attribute-only update.
pub(crate) fn finish_frame(buffer: &Buffer, hyperlinks_visible: bool) {
    HYPERLINK_FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        for y in buffer.area.top()..buffer.area.bottom() {
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let width = buffer[(x, y)].symbol().width().max(1);
                for continuation in 1..width {
                    let continuation =
                        x.saturating_add(u16::try_from(continuation).unwrap_or(u16::MAX));
                    if continuation < buffer.area.right() {
                        frame.covered.insert((continuation, y));
                    }
                }
                x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
            }
        }
        if !hyperlinks_visible {
            frame.targets.clear();
            return;
        }
        let targets = std::mem::take(&mut frame.targets);
        for (position @ (x, y), target) in targets {
            let cell = &buffer[(x, y)];
            frame.cells.insert(
                position,
                HyperlinkCell {
                    target,
                    cell: cell.clone(),
                },
            );
        }
    });
}

fn take_frame() -> (Hyperlinks, BTreeSet<(u16, u16)>) {
    HYPERLINK_FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        (
            std::mem::take(&mut frame.cells),
            std::mem::take(&mut frame.covered),
        )
    })
}

/// Whether the terminal was last told to show or hide its cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorVisibility {
    Unknown,
    Hidden,
    Shown,
}

/// A backend that makes each ratatui draw reach the terminal as one frame:
/// bracketed in a DEC private mode 2026 (synchronized output) update, with the
/// cursor hidden across the diff, and with cursor traffic written only when it
/// changes something. The three are one concern -- what the terminal is
/// allowed to present between two frames -- so they live together.
///
/// The bracket means the terminal presents the whole frame in a single
/// repaint: no intermediate cursor position is ever displayed and frame
/// tearing goes with it. Terminals that do not implement mode 2026 ignore both
/// sequences, which costs them nothing. The bracket is written into the same
/// buffer as the diff and ended in ratatui's own end-of-draw flush, so a frame
/// is one write rather than a Begin, a diff, and an End each flushed apart.
///
/// ratatui re-emits Show and MoveTo on every draw, and the run loop redraws
/// every 32 ms while anything animates. Terminals restart the cursor blink
/// phase on both sequences, so the composer caret is put back in its lit phase
/// far more often than any blink interval and never goes dark. Remembering what
/// was last written lets an unchanged caret cost nothing at all.
///
/// A frame that repaints anything hides the cursor ahead of its diff. That is
/// what makes the synchronized-output bracket safe on terminals that ignore
/// mode 2026 -- legacy conhost among them -- where the diff is otherwise
/// presented cell by cell with the caret riding along. The known cost: on such
/// a terminal, one that also restarts the blink phase on Show, the caret looks
/// solid while a spinner animates, since every repainting frame hides and
/// re-shows it. Idle redraws (empty diff, unchanged caret) still write nothing
/// and keep blinking.
///
/// Cursor sequences are written here rather than through the inner backend's
/// own cursor methods: `CrosstermBackend` writes those with `execute!`, and
/// each flush would split the frame into another write.
pub(super) struct FrameBackend<B> {
    inner: B,
    frame_open: bool,
    visibility: CursorVisibility,
    /// The last MoveTo written, or `None` when the terminal's cursor is
    /// somewhere else: unknown at start, or displaced by the diff's own moves.
    position: Option<Position>,
    hyperlinks: Hyperlinks,
}

impl<B> FrameBackend<B> {
    pub(super) fn new(inner: B) -> Self {
        Self {
            inner,
            frame_open: false,
            visibility: CursorVisibility::Unknown,
            position: None,
            hyperlinks: BTreeMap::new(),
        }
    }

    /// Forgets what the terminal was last told, so the next frame re-asserts
    /// both visibility and position. Called whenever the display is entered,
    /// because entering writes its own Hide through the sink, past this memory.
    pub(super) fn forget_cursor(&mut self) {
        self.visibility = CursorVisibility::Unknown;
        self.position = None;
        self.hyperlinks.clear();
    }

    #[cfg(test)]
    pub(super) fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: Write> FrameBackend<B> {
    /// Ends the frame's synchronized update and pushes the frame out, if a
    /// frame is open. A frame that fails before ratatui reaches its flush is
    /// still ended, for the same reason the display restores past a failure:
    /// a terminal left inside a synchronized update shows nothing further --
    /// not even the sequences that give the screen back -- until its own
    /// timeout expires.
    pub(super) fn end_frame(&mut self) -> std::io::Result<()> {
        if self.frame_open {
            self.queue(EndSynchronizedUpdate)?;
            self.frame_open = false;
            self.inner.flush()?;
        }
        Ok(())
    }

    /// Writes a command's ANSI rendering into the buffer without flushing,
    /// which `execute!` would do. The rendering is always ANSI: termina's raw
    /// mode enables virtual terminal processing on Windows, so the console
    /// API fallback `queue!` would pick without it is never needed.
    fn queue(&mut self, command: impl Command) -> std::io::Result<()> {
        self.inner.write_all(ansi(&command)?.as_bytes())
    }
}

/// A command as the bytes an ANSI terminal receives, independent of the
/// console the process is attached to.
pub(super) fn ansi(command: &impl Command) -> std::io::Result<String> {
    let mut rendered = String::new();
    command
        .write_ansi(&mut rendered)
        .map_err(std::io::Error::other)?;
    Ok(rendered)
}

impl<B: Backend + Write> Backend for FrameBackend<B> {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Begin has to reach the terminal ahead of everything else in the
        // frame, the cursor Hide included. It is written even when there is
        // nothing to draw, so an idle frame is still a whole frame.
        if !self.frame_open {
            self.queue(BeginSynchronizedUpdate)?;
            self.frame_open = true;
        }
        let (current_hyperlinks, covered) = take_frame();
        let size = self.inner.size()?;
        let mut cells = content
            .map(|(x, y, cell)| ((x, y), cell.clone()))
            .collect::<BTreeMap<_, _>>();
        let changed_targets = self
            .hyperlinks
            .keys()
            .chain(current_hyperlinks.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        for position in changed_targets {
            if position.0 >= size.width || position.1 >= size.height {
                continue;
            }
            if covered.contains(&position) {
                continue;
            }
            let old = self.hyperlinks.get(&position).map(|cell| &cell.target);
            let new = current_hyperlinks.get(&position).map(|cell| &cell.target);
            if old != new {
                let cell = current_hyperlinks
                    .get(&position)
                    .or_else(|| self.hyperlinks.get(&position))
                    .expect("a changed hyperlink has an old or new cell")
                    .cell
                    .clone();
                cells.entry(position).or_insert(cell);
            }
        }
        let mut content = cells.iter().peekable();
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
        let mut cells = content
            .map(|(position, cell)| (*position, cell))
            .collect::<Vec<_>>();
        cells.sort_unstable_by_key(|((x, y), _)| (*y, *x));
        let mut start = 0;
        while start < cells.len() {
            let target = current_hyperlinks
                .get(&cells[start].0)
                .map(|link| link.target.as_str());
            let mut end = start + 1;
            while end < cells.len()
                && current_hyperlinks
                    .get(&cells[end].0)
                    .map(|link| link.target.as_str())
                    == target
            {
                end += 1;
            }
            if let Some(target) = target {
                self.inner
                    .write_all(format!("\x1b]8;;{target}\x1b\\").as_bytes())?;
            }
            let drawn = self.inner.draw(
                cells[start..end]
                    .iter()
                    .map(|((x, y), cell)| (*x, *y, *cell)),
            );
            let closed = if target.is_some() {
                self.inner.write_all(b"\x1b]8;;\x1b\\")
            } else {
                Ok(())
            };
            drawn.and(closed)?;
            start = end;
        }
        self.hyperlinks = current_hyperlinks;
        Ok(())
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        if self.visibility != CursorVisibility::Hidden {
            self.queue(Hide)?;
            self.visibility = CursorVisibility::Hidden;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        if self.visibility != CursorVisibility::Shown {
            self.queue(Show)?;
            self.visibility = CursorVisibility::Shown;
        }
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        let position = position.into();
        if self.position != Some(position) {
            self.queue(MoveTo(position.x, position.y))?;
            self.position = Some(position);
        }
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.position = None;
        self.hyperlinks.clear();
        self.inner.clear()
    }

    /// Clearing a region leaves the cursor where it was, unlike `clear` and
    /// `append_lines`, so the remembered position stays good.
    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.position = None;
        self.hyperlinks.clear();
        self.inner.append_lines(n)
    }

    fn size(&self) -> std::io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        self.inner.window_size()
    }

    /// ratatui flushes once, at the end of a draw, after it has parked the
    /// cursor: the right moment to end the frame, so End lands last.
    fn flush(&mut self) -> std::io::Result<()> {
        self.end_frame()?;
        Backend::flush(&mut self.inner)
    }
}

/// Raw writes go straight through: the colour probe and the clipboard
/// replies are written on the backend, not drawn through it.
impl<B: Write> Write for FrameBackend<B> {
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
