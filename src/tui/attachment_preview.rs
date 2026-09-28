//! Thumbnails of Attachments: a strip of them stands above the composer's
//! text and beneath a user Message's text in the Transcript, drawn through
//! the graphics protocol the terminal was probed for (ADR 0038).
//!
//! A strip stands in rows reserved at [`STRIP_ROWS`], so the layout never
//! moves when a thumbnail arrives or a strip may not be drawn: until its
//! thumbnails are ready its Attachments' dimmed lines fill those rows, and
//! where a strip is not wholly in view or something covers it the rows are
//! left blank, because an image cannot be clipped and any cell written over
//! one corrupts it. Where the Setting is off or the terminal draws no image,
//! nothing is reserved and the dimmed lines stand as they always have.
//!
//! The client makes thumbnails itself, off the UI thread: it fetches an
//! Attachment's bytes by id, decodes them as one of the four admitted
//! formats, scales them to a strip's height at the probed cell size, and
//! encodes them for the protocol through ratatui-image, which is told the
//! protocol and cell size rather than probing for either. The cache holds the
//! open Session's Attachments only, keyed by id and cell size.

use std::{cell::RefCell, collections::HashMap};

use anyhow::{Context, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Position, Rect},
    text::Line,
    widgets::{Block, Paragraph, Widget},
};
use ratatui_image::{
    Image,
    protocol::{Protocol, iterm2::Iterm2, kitty::Kitty, sixel::Sixel},
};
use unicode_width::UnicodeWidthStr;

use super::{
    commands::{SemanticCommandId, SemanticInvocation},
    text_binding::TextBindings,
};
use crate::{
    protocol::{AttachmentDescriptor, AttachmentId, SessionId},
    terminal::{CellSize, GraphicsProtocol},
    theme::Theme,
};

/// The rows a strip stands in, whether it draws thumbnails, the dimmed lines
/// that stand in for them, or nothing.
pub(super) const STRIP_ROWS: u16 = 6;

/// The thumbnails a strip draws before it counts the rest as `+N more`.
const STRIP_THUMBNAILS: usize = 3;

/// The widest a thumbnail is drawn, in columns, however wide its image.
const THUMBNAIL_MAX_COLUMNS: u16 = 24;

/// The blank column between two parts of a strip.
const STRIP_GAP: u16 = 1;

/// The formats a thumbnail is decoded from: the four an Attachment is admitted
/// as (ADR 0037), whatever else the build happens to be able to read.
const PREVIEWED_FORMATS: [ImageFormat; 4] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Gif,
    ImageFormat::WebP,
];

/// One Attachment scaled to a strip's height at one cell size and encoded for
/// one graphics protocol, ready to be drawn without further work.
#[derive(Clone)]
pub struct Thumbnail {
    columns: u16,
    rows: u16,
    protocol: Protocol,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Thumbnail")
            .field("columns", &self.columns)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl Thumbnail {
    /// Decodes an Attachment's stored bytes and makes its thumbnail. Only the
    /// four admitted formats are read; any other is refused before a decoder
    /// sees it.
    pub fn decode(
        bytes: &[u8],
        cell_size: CellSize,
        protocol: GraphicsProtocol,
    ) -> anyhow::Result<Self> {
        let format = image::guess_format(bytes).context("recognize the Attachment's format")?;
        if !PREVIEWED_FORMATS.contains(&format) {
            bail!("an Attachment stored as {format:?} is not previewed");
        }
        let image =
            image::load_from_memory_with_format(bytes, format).context("decode the Attachment")?;
        Self::from_image(image, cell_size, protocol)
    }

    /// Scales `image` to a strip's height at `cell_size` — as wide as its
    /// aspect ratio dictates, up to [`THUMBNAIL_MAX_COLUMNS`] — and encodes it
    /// for `protocol`.
    pub fn from_image(
        image: DynamicImage,
        cell_size: CellSize,
        protocol: GraphicsProtocol,
    ) -> anyhow::Result<Self> {
        if cell_size.width == 0 || cell_size.height == 0 {
            bail!("a thumbnail needs a cell size in pixels");
        }
        let (width, height) = thumbnail_pixels(image.width(), image.height(), cell_size);
        let scaled = image.resize_exact(width, height, FilterType::Triangle);
        let columns = cells(width, cell_size.width).min(THUMBNAIL_MAX_COLUMNS);
        let rows = cells(height, cell_size.height).min(STRIP_ROWS);
        let area = Rect::new(0, 0, columns, rows);
        // Suru never draws under a multiplexer, so nothing is wrapped for one.
        let protocol = match protocol {
            GraphicsProtocol::Kitty => {
                Protocol::Kitty(Kitty::new(scaled, area, kitty_image_id(), false)?)
            }
            GraphicsProtocol::Iterm2 => Protocol::ITerm2(Iterm2::new(scaled, area, false)?),
            GraphicsProtocol::Sixel => Protocol::Sixel(Sixel::new(scaled, area, false)?),
        };
        Ok(Self {
            columns,
            rows,
            protocol,
        })
    }

    /// The columns the thumbnail is drawn across.
    pub fn columns(&self) -> u16 {
        self.columns
    }

    /// The rows the thumbnail is drawn down, never more than a strip's.
    pub fn rows(&self) -> u16 {
        self.rows
    }
}

/// The pixel size a thumbnail of a `width` by `height` image is scaled to: a
/// strip's height, or less where the width cap binds first.
fn thumbnail_pixels(width: u32, height: u32, cell_size: CellSize) -> (u32, u32) {
    let (width, height) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let tallest = u64::from(STRIP_ROWS) * u64::from(cell_size.height);
    let widest = u64::from(THUMBNAIL_MAX_COLUMNS) * u64::from(cell_size.width);
    let scaled_width = (width * tallest + height / 2) / height;
    let (scaled_width, scaled_height) = if scaled_width <= widest {
        (scaled_width, tallest)
    } else {
        (widest, (height * widest + width / 2) / width)
    };
    (
        u32::try_from(scaled_width.max(1)).unwrap_or(u32::MAX),
        u32::try_from(scaled_height.max(1)).unwrap_or(u32::MAX),
    )
}

/// The cells `pixels` span at `cell` pixels a cell, a partial cell counting
/// whole.
fn cells(pixels: u32, cell: u16) -> u16 {
    u16::try_from(pixels.div_ceil(u32::from(cell)))
        .unwrap_or(u16::MAX)
        .max(1)
}

/// A Kitty image id unlikely to collide with another program's in the same
/// terminal, as ratatui-image's own are: never zero, which Kitty reserves.
fn kitty_image_id() -> u32 {
    (uuid::Uuid::new_v4().as_u128() as u32).max(1)
}

/// How Attachments present, as the Setting and the terminal allow.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(super) enum PreviewMode {
    /// As dimmed lines: the Setting is off, or the terminal draws no image.
    #[default]
    Lines,
    /// As strips of thumbnails at `cell_size`, encoded for `protocol`.
    Strips {
        cell_size: CellSize,
        protocol: GraphicsProtocol,
    },
}

impl PreviewMode {
    /// Strips where the Setting asks for previews and the terminal was found
    /// to draw images, whose cell size is then known; lines otherwise.
    pub(super) fn of(
        previews_on: bool,
        graphics: Option<GraphicsProtocol>,
        cell_size: Option<CellSize>,
    ) -> Self {
        match (previews_on, graphics, cell_size) {
            (true, Some(protocol), Some(cell_size)) => Self::Strips {
                cell_size,
                protocol,
            },
            _ => Self::Lines,
        }
    }
}

/// How a Prompt or Message presents the Attachments it binds: the one typed
/// item the Transcript projects beneath a user Message's text and the composer
/// draws with its own, so neither decides for itself whether a preview stands.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum AttachmentRows {
    /// One dimmed line per Attachment, as they stand wherever no preview may
    /// be drawn.
    Lines(Vec<String>),
    /// A strip of thumbnails, in rows reserved at [`STRIP_ROWS`].
    Strip(AttachmentStrip),
}

impl AttachmentRows {
    /// How many rows the Attachments take, beside the text they are bound to.
    pub(super) fn row_count(&self) -> usize {
        match self {
            Self::Lines(lines) => lines.len(),
            Self::Strip(_) => usize::from(STRIP_ROWS),
        }
    }
}

/// One strip: every Attachment a Prompt or Message binds, the lines that
/// describe them, and whether the thumbnails it shows are ready.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct AttachmentStrip {
    /// Every Attachment bound beside the text, in text order.
    attachments: Vec<AttachmentId>,
    /// The dimmed line describing each, which fill the strip's first rows
    /// until its thumbnails are ready.
    lines: Vec<String>,
    /// Whether the thumbnail of every Attachment the strip shows is ready.
    ready: bool,
}

impl AttachmentStrip {
    /// The Attachments the strip draws thumbnails of; it counts the rest.
    pub(super) fn shown(&self) -> &[AttachmentId] {
        &self.attachments[..self.attachments.len().min(STRIP_THUMBNAILS)]
    }

    pub(super) fn attachments(&self) -> &[AttachmentId] {
        &self.attachments
    }

    /// The dimmed lines, one per Attachment, standing in the strip's rows
    /// while its thumbnails are not ready.
    pub(super) fn lines(&self) -> &[String] {
        &self.lines
    }

    pub(super) fn is_ready(&self) -> bool {
        self.ready
    }
}

/// Where a thumbnail stands in the cache for the open Session.
#[derive(Clone)]
enum Entry {
    /// Asked for, and not answered yet: one fetch per Attachment at a time.
    Fetching,
    Ready(Thumbnail),
    /// The fetch or the decode failed, so the dimmed line stands for good.
    Failed,
}

/// A strip whose thumbnails are ready, reserved at a place in the frame and
/// waiting on the frame's end to learn whether it may be drawn there.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReservedStrip {
    /// The strip's first row, in frame coordinates. It stands above the frame
    /// where the Transcript has scrolled the strip's top away.
    pub(super) top: i32,
    pub(super) left: u16,
    pub(super) width: u16,
    /// The area that shows the strip: the Transcript's rows, or the
    /// composer's.
    pub(super) viewport: Rect,
    pub(super) attachments: Vec<AttachmentId>,
}

/// What one frame learned about strips: which Attachments it wanted
/// thumbnails of, which strips it reserved, what it drew over them, and where
/// it drew each thumbnail — the target a press on it lands on until the next
/// frame draws again.
#[derive(Clone, Debug, Default)]
struct PreviewFrame {
    wanted: Vec<AttachmentId>,
    reserved: Vec<ReservedStrip>,
    covers: Vec<Rect>,
    targets: Vec<(Rect, AttachmentId)>,
}

/// The thumbnails of the open Session's Attachments, and what the current
/// frame is doing with them. A frame renders from shared state, so it records
/// what it wanted and reserved through interior mutability, as the Transcript
/// cache does.
#[derive(Clone, Default)]
pub(super) struct AttachmentPreviews {
    mode: PreviewMode,
    /// The Session whose Attachments the cache holds; `None` is the Landing.
    scope: Option<SessionId>,
    thumbnails: HashMap<(AttachmentId, CellSize), Entry>,
    /// Moves whenever what a strip presents could have changed, so the
    /// Transcript's cached view knows to look again.
    generation: u64,
    frame: RefCell<PreviewFrame>,
}

impl std::fmt::Debug for AttachmentPreviews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachmentPreviews")
            .field("mode", &self.mode)
            .field("scope", &self.scope)
            .field("thumbnails", &self.thumbnails.len())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl AttachmentPreviews {
    /// Holds only `scope`'s thumbnails, presented as `mode` says: opening
    /// another Session, returning to the Landing, a cell size changing, or
    /// the Setting being turned drops every thumbnail held.
    pub(super) fn hold_for(&mut self, scope: Option<SessionId>, mode: PreviewMode) {
        if self.scope == scope && self.mode == mode {
            return;
        }
        self.scope = scope;
        self.mode = mode;
        self.thumbnails.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// Hands the thumbnails held to the same Session under another identity:
    /// a Landing draft's to the claim its Prompt begins, and a claim's to the
    /// Session that answers it. A Session being born is not another Session,
    /// so nothing it already showed is fetched again.
    pub(super) fn carry_to(&mut self, scope: Option<SessionId>) {
        self.scope = scope;
    }

    /// Changes whenever a strip might present differently, which is what a
    /// cached projection of one is keyed on.
    pub(super) fn fingerprint(&self) -> u64 {
        self.generation
    }

    /// How the Attachments `bindings` bind present beside their text: as
    /// dimmed lines where no preview may be drawn, and as a strip otherwise.
    pub(super) fn rows<'a>(
        &self,
        bindings: &TextBindings,
        describe: impl Fn(&AttachmentId) -> Option<&'a AttachmentDescriptor>,
    ) -> AttachmentRows {
        let lines = bindings.attachment_lines(describe);
        if lines.is_empty() || self.mode == PreviewMode::Lines {
            return AttachmentRows::Lines(lines);
        }
        let attachments = bindings
            .attachments()
            .map(|(_, attachment)| attachment.attachment_id().clone())
            .collect::<Vec<_>>();
        let ready = attachments
            .iter()
            .take(STRIP_THUMBNAILS)
            .all(|id| self.thumbnail(id).is_some());
        AttachmentRows::Strip(AttachmentStrip {
            attachments,
            lines,
            ready,
        })
    }

    fn thumbnail(&self, id: &AttachmentId) -> Option<&Thumbnail> {
        let PreviewMode::Strips { cell_size, .. } = self.mode else {
            return None;
        };
        match self.thumbnails.get(&(id.clone(), cell_size)) {
            Some(Entry::Ready(thumbnail)) => Some(thumbnail),
            _ => None,
        }
    }

    /// Forgets what the last frame recorded, before this one records afresh.
    pub(super) fn begin_frame(&self) {
        let mut frame = self.frame.borrow_mut();
        frame.wanted.clear();
        frame.reserved.clear();
        frame.covers.clear();
        frame.targets.clear();
    }

    /// Records that this frame drew `strip` in view, so the thumbnails it
    /// shows are fetched where none is held or coming.
    pub(super) fn want(&self, strip: &AttachmentStrip) {
        let PreviewMode::Strips { cell_size, .. } = self.mode else {
            return;
        };
        let mut frame = self.frame.borrow_mut();
        for id in strip.shown() {
            if !self.thumbnails.contains_key(&(id.clone(), cell_size)) && !frame.wanted.contains(id)
            {
                frame.wanted.push(id.clone());
            }
        }
    }

    /// Records a strip whose thumbnails are ready, to be drawn at the frame's
    /// end if it is wholly in view and nothing covers it.
    pub(super) fn reserve(&self, strip: ReservedStrip) {
        self.frame.borrow_mut().reserved.push(strip);
    }

    /// Records an area something was drawn over after the surfaces that
    /// reserve strips: a side column, a picker, a dialog, a notice.
    pub(super) fn cover(&self, area: Rect) {
        self.frame.borrow_mut().covers.push(area);
    }

    /// The next Attachment the last frame wanted a thumbnail of and none is
    /// held or coming for, marked as coming, with the cell size and protocol
    /// its thumbnail is made for.
    pub(super) fn take_fetch(&mut self) -> Option<(AttachmentId, CellSize, GraphicsProtocol)> {
        let frame = self.frame.get_mut();
        let PreviewMode::Strips {
            cell_size,
            protocol,
        } = self.mode
        else {
            frame.wanted.clear();
            return None;
        };
        while !frame.wanted.is_empty() {
            let id = frame.wanted.remove(0);
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.thumbnails.entry((id.clone(), cell_size))
            {
                entry.insert(Entry::Fetching);
                return Some((id, cell_size, protocol));
            }
        }
        None
    }

    /// Takes a fetch's answer: the thumbnail, or `None` where the fetch or
    /// the decode failed. An answer nothing is still waiting on — one for a
    /// Session no longer open, or a cell size no longer current — is dropped.
    pub(super) fn receive(
        &mut self,
        id: AttachmentId,
        cell_size: CellSize,
        thumbnail: Option<Thumbnail>,
    ) {
        let Some(entry @ Entry::Fetching) = self.thumbnails.get_mut(&(id, cell_size)) else {
            return;
        };
        *entry = thumbnail.map_or(Entry::Failed, Entry::Ready);
        self.generation = self.generation.wrapping_add(1);
    }

    /// What a press at `position` invokes, where the last frame drew a
    /// thumbnail there: `attachment.open` on the Attachment it shows.
    pub(super) fn press_at(&self, position: Position) -> Option<SemanticInvocation> {
        self.frame
            .borrow()
            .targets
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, id)| SemanticCommandId::AttachmentOpen.on_attachment(id.clone()))
    }

    /// Draws every strip this frame reserved that is wholly in view and
    /// uncovered, now that everything else has been drawn; the rest keep the
    /// blank rows they were reserved as.
    pub(super) fn draw(&self, buffer: &mut Buffer, theme: &Theme) {
        let mut frame = self.frame.borrow_mut();
        let PreviewFrame {
            reserved,
            covers,
            targets,
            ..
        } = &mut *frame;
        for strip in reserved.iter() {
            let Some(area) =
                drawable_strip_area(strip.top, strip.left, strip.width, strip.viewport, covers)
            else {
                continue;
            };
            let Some(thumbnails) = strip
                .attachments
                .iter()
                .take(STRIP_THUMBNAILS)
                .map(|id| self.thumbnail(id).map(|thumbnail| (id, thumbnail)))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let layout = lay_out_strip(
                area,
                &thumbnails
                    .iter()
                    .map(|(_, thumbnail)| thumbnail.columns)
                    .collect::<Vec<_>>(),
                strip.attachments.len(),
            );
            for (index, cell) in layout.thumbnails.iter().enumerate() {
                let (id, thumbnail) = thumbnails[index];
                let drawn = Rect {
                    height: thumbnail.rows,
                    ..*cell
                };
                Image::new(&thumbnail.protocol).render(drawn, buffer);
                targets.push((drawn, id.clone()));
            }
            if let Some((cell, more)) = layout.more {
                draw_more_cell(buffer, cell, more, theme);
            }
        }
    }
}

/// The area a reserved strip is drawn over this frame: all of its rows, when
/// every one of them stands inside `viewport` and none of its cells lies
/// under any of `covers` — the Sidebar, the Aside, and every picker, dialog,
/// and notice drawn over the frame. `None` leaves its rows blank at their
/// reserved height, since no image is ever drawn clipped (ADR 0038).
pub(super) fn drawable_strip_area(
    top: i32,
    left: u16,
    width: u16,
    viewport: Rect,
    covers: &[Rect],
) -> Option<Rect> {
    let top = u16::try_from(top).ok()?;
    let area = Rect {
        x: left,
        y: top,
        width,
        height: STRIP_ROWS,
    };
    let whole = width > 0
        && u32::from(top) + u32::from(STRIP_ROWS) <= u32::from(viewport.bottom())
        && viewport.intersection(area) == area;
    (whole && !covers.iter().any(|cover| cover.intersects(area))).then_some(area)
}

/// Where the parts of a drawable strip stand.
#[derive(Debug, Eq, PartialEq)]
struct StripLayout {
    /// One cell per thumbnail drawn, a strip's height tall, from the left.
    thumbnails: Vec<Rect>,
    /// The `+N more` cell and its N, where any Attachment went undrawn.
    more: Option<(Rect, usize)>,
}

/// Lays thumbnails of `columns` wide out across `area` from the left, as many
/// as fit whole beside the `+N more` cell counting every Attachment of
/// `total` left undrawn.
fn lay_out_strip(area: Rect, columns: &[u16], total: usize) -> StripLayout {
    let fits = |count: usize| {
        let thumbnails = columns[..count]
            .iter()
            .map(|width| u32::from(*width) + u32::from(STRIP_GAP))
            .sum::<u32>();
        let more = total - count;
        let needed = if more == 0 {
            thumbnails.saturating_sub(u32::from(STRIP_GAP))
        } else {
            thumbnails + u32::from(more_cell_width(more))
        };
        needed <= u32::from(area.width)
    };
    let count = (0..=columns.len().min(total))
        .rev()
        .find(|count| fits(*count))
        .unwrap_or(0);
    let mut x = area.x;
    let mut thumbnails = Vec::with_capacity(count);
    for width in &columns[..count] {
        thumbnails.push(Rect {
            x,
            width: *width,
            ..area
        });
        x = x.saturating_add(*width).saturating_add(STRIP_GAP);
    }
    let more = total - count;
    let width = more_cell_width(more);
    let more = (more > 0 && x.saturating_add(width) <= area.right())
        .then_some((Rect { x, width, ..area }, more));
    StripLayout { thumbnails, more }
}

/// How the Attachments a strip leaves undrawn are counted.
fn more_label(more: usize) -> String {
    format!("+{more} more")
}

/// The columns the `+N more` cell takes: its label, a column of air either
/// side, and its border.
fn more_cell_width(more: usize) -> u16 {
    u16::try_from(more_label(more).width())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
}

fn draw_more_cell(buffer: &mut Buffer, area: Rect, more: usize, theme: &Theme) {
    let block = Block::bordered().border_style(theme.border.subdued);
    let inner = block.inner(area);
    block.render(area, buffer);
    let middle = Rect {
        y: inner.y + inner.height.saturating_sub(1) / 2,
        height: inner.height.min(1),
        ..inner
    };
    Paragraph::new(Line::styled(more_label(more), theme.text.subdued))
        .alignment(Alignment::Center)
        .render(middle, buffer);
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageBuffer, Rgba, RgbaImage};

    use super::*;

    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
    };

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([200, 40, 40, 255]),
        ));
        // JPEG carries no alpha, so every format is written from the same
        // opaque pixels in the channels it can hold.
        let image = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(image.to_rgb8())
        } else {
            image
        };
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, format)
            .expect("encode the fixture image");
        bytes.into_inner()
    }

    #[test]
    fn a_decoded_thumbnail_is_a_strip_tall_and_as_wide_as_its_aspect_ratio() {
        // 240×120 pixels scale to the strip's 120 pixels, 240 pixels wide at
        // a 10×20 cell: 24 columns by 6 rows, just inside the cap.
        let thumbnail = Thumbnail::decode(
            &encoded(240, 120, ImageFormat::Png),
            CELL,
            GraphicsProtocol::Kitty,
        )
        .expect("a PNG makes a thumbnail");
        assert_eq!((thumbnail.columns(), thumbnail.rows()), (24, 6));

        // A tiny image grows to the strip's height rather than standing in
        // one corner of it.
        let tiny = Thumbnail::decode(
            &encoded(2, 2, ImageFormat::Png),
            CELL,
            GraphicsProtocol::Kitty,
        )
        .expect("a tiny PNG makes a thumbnail");
        assert_eq!((tiny.columns(), tiny.rows()), (12, 6));
    }

    #[test]
    fn every_admitted_format_decodes_and_any_other_is_refused() {
        for format in PREVIEWED_FORMATS {
            let thumbnail =
                Thumbnail::decode(&encoded(40, 40, format), CELL, GraphicsProtocol::Kitty)
                    .unwrap_or_else(|error| panic!("{format:?} makes a thumbnail: {error:#}"));
            assert_eq!(
                (thumbnail.columns(), thumbnail.rows()),
                (12, 6),
                "{format:?}"
            );
        }

        // A BMP header, which some platforms' builds could decode, is refused
        // before any decoder reads it.
        let mut bmp = b"BM".to_vec();
        bmp.resize(64, 0);
        let refused = Thumbnail::decode(&bmp, CELL, GraphicsProtocol::Kitty)
            .expect_err("a BMP is not previewed");
        assert!(
            format!("{refused:#}").contains("not previewed"),
            "{refused:#}"
        );
        assert!(Thumbnail::decode(b"not an image", CELL, GraphicsProtocol::Kitty).is_err());
    }

    #[test]
    fn a_wide_image_is_capped_and_a_tall_one_keeps_the_strip_height() {
        assert_eq!(thumbnail_pixels(1000, 100, CELL), (240, 24));
        assert_eq!(thumbnail_pixels(100, 1000, CELL), (12, 120));
        let wide = Thumbnail::from_image(
            DynamicImage::ImageRgba8(ImageBuffer::new(1000, 100)),
            CELL,
            GraphicsProtocol::Kitty,
        )
        .expect("a wide image makes a thumbnail");
        assert_eq!((wide.columns(), wide.rows()), (THUMBNAIL_MAX_COLUMNS, 2));
    }

    #[test]
    fn a_strip_is_drawable_only_whole_inside_its_viewport_and_uncovered() {
        let viewport = Rect::new(0, 2, 60, 20);
        let drawn = Rect::new(4, 5, 40, STRIP_ROWS);
        assert_eq!(drawable_strip_area(5, 4, 40, viewport, &[]), Some(drawn));
        // Its first row scrolled above the viewport, or above the frame.
        assert_eq!(drawable_strip_area(1, 4, 40, viewport, &[]), None);
        assert_eq!(drawable_strip_area(-2, 4, 40, viewport, &[]), None);
        // Its last row past the viewport's bottom.
        assert_eq!(drawable_strip_area(17, 4, 40, viewport, &[]), None);
        assert_eq!(
            drawable_strip_area(16, 4, 40, viewport, &[]),
            Some(Rect::new(4, 16, 40, STRIP_ROWS))
        );
        // A Sidebar, an Aside, a picker, or a dialog over any of its cells.
        for cover in [
            Rect::new(0, 0, 5, 30),
            Rect::new(43, 0, 20, 30),
            Rect::new(20, 10, 10, 3),
            Rect::new(0, 0, 80, 30),
        ] {
            assert_eq!(
                drawable_strip_area(5, 4, 40, viewport, &[cover]),
                None,
                "{cover:?}"
            );
        }
        // Something beside it covers nothing of it.
        assert_eq!(
            drawable_strip_area(5, 4, 40, viewport, &[Rect::new(44, 5, 10, 6)]),
            Some(drawn)
        );
    }

    #[test]
    fn a_strip_lays_out_three_thumbnails_then_counts_the_rest() {
        let area = Rect::new(2, 0, 70, STRIP_ROWS);
        let layout = lay_out_strip(area, &[12, 20, 8], 4);
        assert_eq!(
            layout.thumbnails,
            vec![
                Rect::new(2, 0, 12, STRIP_ROWS),
                Rect::new(15, 0, 20, STRIP_ROWS),
                Rect::new(36, 0, 8, STRIP_ROWS),
            ]
        );
        assert_eq!(layout.more, Some((Rect::new(45, 0, 11, STRIP_ROWS), 1)));

        // Too narrow for all three: the one that does not fit is counted.
        let narrow = lay_out_strip(Rect::new(0, 0, 46, STRIP_ROWS), &[12, 20, 16], 3);
        assert_eq!(narrow.thumbnails.len(), 2);
        assert_eq!(narrow.more.map(|(_, more)| more), Some(1));

        // Every thumbnail drawn leaves nothing to count.
        let all = lay_out_strip(area, &[12, 12], 2);
        assert_eq!(all.thumbnails.len(), 2);
        assert_eq!(all.more, None);
    }

    #[test]
    fn a_press_on_a_drawn_thumbnail_invokes_attachment_open_on_its_attachment() {
        let screenshot = AttachmentId::new("screenshot-hash");
        let diagram = AttachmentId::new("diagram-hash");
        let mut previews = AttachmentPreviews::default();
        previews.hold_for(
            None,
            PreviewMode::Strips {
                cell_size: CELL,
                protocol: GraphicsProtocol::Kitty,
            },
        );
        let viewport = Rect::new(0, 0, 60, 10);
        let strip = ReservedStrip {
            top: 2,
            left: 4,
            width: 50,
            viewport,
            attachments: vec![screenshot.clone(), diagram.clone()],
        };
        let mut bindings = TextBindings::default();
        bindings.bind_attachment(0..9, screenshot.clone(), "[Image 1]".to_owned());
        bindings.bind_attachment(10..19, diagram.clone(), "[Image 2]".to_owned());
        let AttachmentRows::Strip(waiting) = previews.rows(&bindings, |_| None) else {
            panic!("previews on present a strip");
        };
        previews.begin_frame();
        previews.want(&waiting);
        while let Some((id, cell_size, _)) = previews.take_fetch() {
            let thumbnail = Thumbnail::from_image(
                DynamicImage::ImageRgba8(ImageBuffer::new(120, 120)),
                cell_size,
                GraphicsProtocol::Kitty,
            )
            .expect("make a thumbnail");
            previews.receive(id, cell_size, Some(thumbnail));
        }

        previews.begin_frame();
        previews.reserve(strip);
        previews.draw(&mut Buffer::empty(viewport), &Theme::system());
        assert_eq!(
            previews.press_at(Position::new(4, 2)),
            Some(SemanticCommandId::AttachmentOpen.on_attachment(screenshot))
        );
        assert_eq!(
            previews.press_at(Position::new(4 + 13 + 11, 7)),
            Some(SemanticCommandId::AttachmentOpen.on_attachment(diagram))
        );
        // The gap between them, and the rows beneath, open nothing.
        assert_eq!(previews.press_at(Position::new(16, 3)), None);
        assert_eq!(previews.press_at(Position::new(4, 8)), None);
    }

    #[test]
    fn a_kitty_thumbnail_marks_its_first_cell_and_skips_the_rest_of_each_row() {
        let thumbnail = Thumbnail::from_image(
            DynamicImage::ImageRgba8(ImageBuffer::new(120, 120)),
            CELL,
            GraphicsProtocol::Kitty,
        )
        .expect("make a thumbnail");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, STRIP_ROWS));
        Image::new(&thumbnail.protocol).render(Rect::new(1, 0, 12, STRIP_ROWS), &mut buffer);
        for y in 0..STRIP_ROWS {
            assert!(buffer[(1, y)].symbol().contains('\u{10EEEE}'), "row {y}");
            assert!((2..13).all(|x| buffer[(x, y)].skip), "row {y}");
            assert!(!buffer[(0, y)].skip && !buffer[(13, y)].skip, "row {y}");
        }
    }
}
