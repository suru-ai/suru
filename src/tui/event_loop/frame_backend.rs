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

/// The graphics payloads written to the terminal and not drawn over since,
/// each by the cell it was written from.
///
/// ratatui's diff measures a cell by its symbol's display width, and a
/// payload -- thousands of printable characters of PNG or Sixel data, or a
/// row of Kitty placeholders wrapped in escapes -- measures far wider than
/// the cells it stands in. ratatui counts every cell within that width after
/// it as displaced and hands it over again on every draw: for iTerm2 and
/// Sixel the rest of the frame, every later thumbnail's whole payload among
/// it, and for Kitty the placeholder rows of a thumbnail standing close
/// after another's. A payload handed over again unchanged, at a cell where
/// the terminal still shows what it drew, is not written.
///
/// Everything is forgotten whenever the screen is cleared, which ratatui
/// does before the first draw at a new size.
#[derive(Default)]
struct SentGraphics {
    /// Keyed row first, so they order as a frame is written.
    cells: BTreeMap<(u16, u16), Cell>,
}

impl SentGraphics {
    fn forget(&mut self) {
        self.cells.clear();
    }

    /// Whether `cell` has to be written at `x`, `y`, noting what writing it
    /// paints over. A frame's cells are asked about in the order they are
    /// written.
    fn must_write(&mut self, (x, y): (u16, u16), cell: &Cell) -> bool {
        if !carries_graphics(cell) {
            // Text over a payload's cell, or over it from a wide glyph beside
            // it, means the image is gone until it is sent again.
            let width = u16::try_from(cell.symbol().width().max(1)).unwrap_or(u16::MAX);
            for column in x..x.saturating_add(width) {
                self.cells.remove(&(y, column));
            }
            return true;
        }
        if self.cells.get(&(y, x)) == Some(cell) {
            return false;
        }
        // A payload paints rightward and downward from its cell, so it may
        // have drawn over any payload later in the frame's order -- the one
        // a strip scrolled a few rows up left behind, whose cell is now
        // skipped and so never written over as text.
        self.cells.retain(|position, _| *position < (y, x));
        self.cells.insert((y, x), cell.clone());
        true
    }
}

/// Whether a cell carries a graphics payload ratatui-image set as its symbol
/// -- a row of Kitty placeholders, or a whole iTerm2 or Sixel image -- rather
/// than text. Every payload opens with an escape, and text never does:
/// ratatui drops control characters from every string it sets, and the
/// Transcript sanitizes what it lays out itself.
fn carries_graphics(cell: &Cell) -> bool {
    cell.symbol().starts_with('\x1b')
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
///
/// A thumbnail's payload is written once and then only when something else
/// was drawn over its cell, the screen was cleared, or the terminal resized:
/// ratatui would otherwise hand every thumbnail after the first in a frame
/// over again on every draw, as [`SentGraphics`] explains.
pub(super) struct FrameBackend<B> {
    inner: B,
    frame_open: bool,
    visibility: CursorVisibility,
    /// The last MoveTo written, or `None` when the terminal's cursor is
    /// somewhere else: unknown at start, or displaced by the diff's own moves.
    position: Option<Position>,
    hyperlinks: Hyperlinks,
    graphics: SentGraphics,
}

impl<B> FrameBackend<B> {
    pub(super) fn new(inner: B) -> Self {
        Self {
            inner,
            frame_open: false,
            visibility: CursorVisibility::Unknown,
            position: None,
            hyperlinks: BTreeMap::new(),
            graphics: SentGraphics::default(),
        }
    }

    /// Forgets what the terminal was last told, so the next frame re-asserts
    /// both visibility and position and sends every image it draws again.
    /// Called whenever the display is entered, because entering writes its
    /// own Hide through the sink, past this memory, onto a screen that shows
    /// none of the last frame.
    pub(super) fn forget_cursor(&mut self) {
        self.visibility = CursorVisibility::Unknown;
        self.position = None;
        self.hyperlinks.clear();
        self.graphics.forget();
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

impl<B: Backend + Write> FrameBackend<B> {
    /// Writes a frame's cells in order, each run bound for one hyperlink
    /// target wrapped in it.
    fn write_cells(
        &mut self,
        cells: &[((u16, u16), Cell)],
        current_hyperlinks: &Hyperlinks,
    ) -> std::io::Result<()> {
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
                    .map(|((x, y), cell)| (*x, *y, cell)),
            );
            let closed = if target.is_some() {
                self.inner.write_all(b"\x1b]8;;\x1b\\")
            } else {
                Ok(())
            };
            drawn.and(closed)?;
            start = end;
        }
        Ok(())
    }
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
        let mut cells = cells.into_iter().collect::<Vec<_>>();
        cells.sort_unstable_by_key(|((x, y), _)| (*y, *x));
        // In the order they are written, since what one paints over decides
        // whether a payload after it has to be sent again.
        cells.retain(|(position, cell)| self.graphics.must_write(*position, cell));
        if !cells.is_empty() {
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
        if let Err(error) = self.write_cells(&cells, &current_hyperlinks) {
            // What reached the terminal is unknown, so every image is sent
            // again.
            self.graphics.forget();
            return Err(error);
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
        self.graphics.forget();
        self.inner.clear()
    }

    /// Clearing a region leaves the cursor where it was, unlike `clear` and
    /// `append_lines`, so the remembered position stays good. Any image in
    /// the region goes with it; ratatui clears the whole screen this way
    /// whenever the terminal resizes.
    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.graphics.forget();
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.position = None;
        self.hyperlinks.clear();
        self.graphics.forget();
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

#[cfg(test)]
mod tests {
    use std::{cell::Cell as Shared, io, rc::Rc};

    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::{
        Frame, Terminal,
        backend::{Backend, WindowSize},
        buffer::Cell,
        layout::{Position, Rect, Size},
        widgets::{Block, Clear},
    };
    use ratatui_image::{
        Image,
        protocol::{Protocol, iterm2::Iterm2, kitty::Kitty, sixel::Sixel},
    };

    use super::FrameBackend;
    use crate::terminal::GraphicsProtocol;

    const PROTOCOLS: [GraphicsProtocol; 3] = [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2,
        GraphicsProtocol::Sixel,
    ];

    /// Room for a strip of two thumbnails with another strip beneath it.
    const SIZE: Size = Size {
        width: 50,
        height: 16,
    };

    /// A terminal as big as its test makes it, keeping every cell it is
    /// asked to draw.
    struct Screen {
        size: Rc<Shared<Size>>,
        drawn: Vec<(u16, u16, String)>,
    }

    impl Backend for Screen {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.drawn
                .extend(content.map(|(x, y, cell)| (x, y, cell.symbol().to_owned())));
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(Position::ORIGIN)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, _: P) -> io::Result<()> {
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size.get())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size.get(),
                pixels: Size::ZERO,
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl io::Write for Screen {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    type ScreenTerminal = Terminal<FrameBackend<Screen>>;

    /// A terminal of [`SIZE`], and the handle that resizes it.
    fn screen() -> (ScreenTerminal, Rc<Shared<Size>>) {
        let size = Rc::new(Shared::new(SIZE));
        let backend = FrameBackend::new(Screen {
            size: Rc::clone(&size),
            drawn: Vec::new(),
        });
        (Terminal::new(backend).expect("start a Terminal"), size)
    }

    /// A thumbnail twelve columns by six rows of an image shaded by `shade`,
    /// encoded for `protocol` as ratatui-image encodes one.
    fn thumbnail(protocol: GraphicsProtocol, shade: u8) -> Protocol {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(120, 120, |x, y| {
            Rgba([x as u8, y as u8, shade, 255])
        }));
        let area = Rect::new(0, 0, 12, 6);
        match protocol {
            GraphicsProtocol::Kitty => {
                Kitty::new(image, area, u32::from(shade) + 1, false).map(Protocol::Kitty)
            }
            GraphicsProtocol::Iterm2 => Iterm2::new(image, area, false).map(Protocol::ITerm2),
            GraphicsProtocol::Sixel => Sixel::new(image, area, false).map(Protocol::Sixel),
        }
        .expect("encode a thumbnail")
    }

    /// Draws `thumbnails` side by side from column 2 of row `top`, a column
    /// apart, as a strip stands them.
    fn strip(frame: &mut Frame, top: u16, thumbnails: &[&Protocol]) {
        for (index, thumbnail) in (0u16..).zip(thumbnails) {
            frame.render_widget(Image::new(thumbnail), Rect::new(2 + 13 * index, top, 12, 6));
        }
    }

    /// The cells thumbnails standing at each of `origins` send their payloads
    /// from, in the order they are written: every row's first for Kitty, and
    /// the top-left alone for iTerm2 and Sixel.
    fn payload_cells(protocol: GraphicsProtocol, origins: &[(u16, u16)]) -> Vec<(u16, u16)> {
        let mut cells = origins
            .iter()
            .flat_map(|&(left, top)| match protocol {
                GraphicsProtocol::Kitty => (top..top + 6).map(|y| (left, y)).collect(),
                GraphicsProtocol::Iterm2 | GraphicsProtocol::Sixel => vec![(left, top)],
            })
            .collect::<Vec<_>>();
        cells.sort_by_key(|&(x, y)| (y, x));
        cells
    }

    /// Draws a frame and answers the graphics payloads it sent, each with
    /// the cell it was sent from, in the order they were written.
    fn sent(
        terminal: &mut ScreenTerminal,
        draw: impl FnOnce(&mut Frame),
    ) -> Vec<(u16, u16, String)> {
        terminal.draw(draw).expect("draw a frame");
        std::mem::take(&mut terminal.backend_mut().inner.drawn)
            .into_iter()
            .filter(|(_, _, symbol)| symbol.starts_with('\x1b'))
            .collect()
    }

    /// The cells of what [`sent`] answers.
    fn cells(sent: Vec<(u16, u16, String)>) -> Vec<(u16, u16)> {
        sent.into_iter().map(|(x, y, _)| (x, y)).collect()
    }

    #[test]
    fn an_unchanged_thumbnail_is_sent_once_however_many_frames_draw_it() {
        for protocol in PROTOCOLS {
            let (mut terminal, _) = screen();
            let (first, second) = (thumbnail(protocol, 1), thumbnail(protocol, 2));
            let drawn = |frame: &mut Frame| {
                frame.render_widget("Working", Rect::new(0, 0, 7, 1));
                strip(frame, 1, &[&first, &second]);
            };

            assert_eq!(
                cells(sent(&mut terminal, drawn)),
                payload_cells(protocol, &[(2, 1), (15, 1)]),
                "{protocol}: the first frame sends both"
            );
            let again = sent(&mut terminal, drawn);
            match protocol {
                // Kitty transmits an image with its first row the first time
                // only, so those rows change once more: placeholders, and no
                // image.
                GraphicsProtocol::Kitty => assert!(
                    again.iter().all(|(.., row)| !row.contains("\x1b_G")),
                    "{protocol}: {again:?}"
                ),
                GraphicsProtocol::Iterm2 | GraphicsProtocol::Sixel => {
                    assert_eq!(cells(again), [], "{protocol}: the second frame sends none");
                }
            }
            assert_eq!(cells(sent(&mut terminal, drawn)), [], "{protocol}: settled");
        }
    }

    #[test]
    fn a_later_thumbnail_is_not_sent_again_while_the_frame_around_it_changes() {
        for protocol in PROTOCOLS {
            let (mut terminal, _) = screen();
            let thumbnails = [1, 2, 3].map(|shade| thumbnail(protocol, shade));
            let drawn = |spinner: &'static str| {
                let thumbnails = &thumbnails;
                move |frame: &mut Frame| {
                    frame.render_widget(spinner, Rect::new(0, 0, 1, 1));
                    strip(frame, 1, &[&thumbnails[0], &thumbnails[1]]);
                    strip(frame, 8, &[&thumbnails[2]]);
                }
            };
            sent(&mut terminal, drawn("⠋"));
            sent(&mut terminal, drawn("⠋"));

            for spinner in ["⠙", "⠹", "⠸", "⠼"] {
                terminal.draw(drawn(spinner)).expect("draw a frame");
                let written = std::mem::take(&mut terminal.backend_mut().inner.drawn);
                assert!(
                    written.contains(&(0, 0, spinner.to_owned())),
                    "{protocol}: the spinner is drawn"
                );
                assert!(
                    written
                        .iter()
                        .all(|(.., symbol)| !symbol.starts_with('\x1b')),
                    "{protocol}: no thumbnail is sent again under {spinner}"
                );
            }
        }
    }

    #[test]
    fn a_thumbnail_drawn_over_is_sent_again_once_uncovered() {
        for protocol in PROTOCOLS {
            let (mut terminal, _) = screen();
            let (first, second) = (thumbnail(protocol, 1), thumbnail(protocol, 2));
            let drawn = |frame: &mut Frame| strip(frame, 1, &[&first, &second]);
            sent(&mut terminal, drawn);
            sent(&mut terminal, drawn);

            // A dialog stands over the strip, which is not drawn beneath it.
            let covered = sent(&mut terminal, |frame: &mut Frame| {
                let dialog = Rect::new(8, 2, 24, 4);
                frame.render_widget(Clear, dialog);
                frame.render_widget(Block::bordered().title("Settings"), dialog);
            });
            assert_eq!(cells(covered), [], "{protocol}");

            assert_eq!(
                cells(sent(&mut terminal, drawn)),
                payload_cells(protocol, &[(2, 1), (15, 1)]),
                "{protocol}: both are sent again once the dialog closes"
            );
            assert_eq!(cells(sent(&mut terminal, drawn)), [], "{protocol}");
        }
    }

    #[test]
    fn a_resize_sends_every_thumbnail_again() {
        for protocol in PROTOCOLS {
            let (mut terminal, size) = screen();
            let (first, second) = (thumbnail(protocol, 1), thumbnail(protocol, 2));
            let drawn = |frame: &mut Frame| strip(frame, 1, &[&first, &second]);
            sent(&mut terminal, drawn);
            sent(&mut terminal, drawn);

            size.set(Size {
                width: SIZE.width + 2,
                ..SIZE
            });
            assert_eq!(
                cells(sent(&mut terminal, drawn)),
                payload_cells(protocol, &[(2, 1), (15, 1)]),
                "{protocol}"
            );
            assert_eq!(cells(sent(&mut terminal, drawn)), [], "{protocol}");
        }
    }

    /// Scrolled a few rows up, a strip's image covers the cell it was sent
    /// from before, which is skipped and so never written over as text; back
    /// where it was, it is sent again all the same.
    #[test]
    fn a_strip_scrolled_away_and_back_is_sent_again() {
        for protocol in PROTOCOLS {
            let (mut terminal, _) = screen();
            let image = thumbnail(protocol, 1);
            let at = |top: u16| {
                let image = &image;
                move |frame: &mut Frame| strip(frame, top, &[image])
            };
            sent(&mut terminal, at(8));
            sent(&mut terminal, at(8));

            assert_eq!(
                cells(sent(&mut terminal, at(5))),
                payload_cells(protocol, &[(2, 5)]),
                "{protocol}"
            );
            assert_eq!(
                cells(sent(&mut terminal, at(8))),
                payload_cells(protocol, &[(2, 8)]),
                "{protocol}"
            );
        }
    }
}
