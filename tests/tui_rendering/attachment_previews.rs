//! Thumbnails of Attachments in a terminal that speaks the Kitty graphics
//! protocol: a strip six rows tall beneath a user Message's text, inside its
//! gutter block, and above the composer's text, drawn only whole and
//! uncovered and otherwise left blank at its reserved height. The run loop's
//! fetches are answered here as it would answer them, with thumbnails made
//! from pixels, so no network or decoder runs.

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use image::DynamicImage;
use ratatui::buffer::Buffer;
use suru::{
    managed_client::SessionEvent,
    protocol::{
        AttachmentBinding, AttachmentDescriptor, AttachmentId, AttachmentKind, EffectiveSettings,
        MessageRole, Outlook, SessionId, SessionRevision, SessionSnapshot, TextSpan,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CellSize, ClipboardRead,
        GraphicsProtocol, SemanticCommandId, TerminalFacts, Thumbnail, ThumbnailRequest,
    },
};

use crate::support::{
    buffer_rows, click_mouse, connected_application_with_terminal_facts, deliver_settings,
    enter_session, failed_session_snapshot, invoke, rendered_application_buffer,
    rendered_application_frame, workspace_dir,
};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 40;
const STRIP_ROWS: usize = 6;

/// A cell of 10×20 pixels, at which a square image is a thumbnail twelve
/// columns wide and six rows tall.
const CELL: CellSize = CellSize {
    width: 10,
    height: 20,
};
const THUMBNAIL_COLUMNS: u16 = 12;

/// The placeholder Kitty's Unicode placements are drawn with.
const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

fn kitty() -> TerminalFacts {
    TerminalFacts::unprobed(true)
        .with_cell_size(Some(CELL))
        .with_graphics_answer(GraphicsProtocol::Kitty)
}

fn descriptor(tag: &str, mime_type: &str, width: u32, height: u32) -> AttachmentDescriptor {
    AttachmentDescriptor {
        id: AttachmentId::new(format!("{tag}-hash")),
        kind: AttachmentKind::Image { width, height },
        mime_type: mime_type.to_owned(),
        byte_length: 312 * 1024,
    }
}

fn screenshot() -> AttachmentDescriptor {
    descriptor("screenshot", "image/png", 1280, 720)
}

fn diagram() -> AttachmentDescriptor {
    descriptor("diagram", "image/gif", 320, 200)
}

fn photo() -> AttachmentDescriptor {
    descriptor("photo", "image/jpeg", 800, 600)
}

fn sketch() -> AttachmentDescriptor {
    descriptor("sketch", "image/webp", 64, 64)
}

/// Binds `label`, found in `text`, to the Attachment `descriptor` describes.
fn bound(descriptor: &AttachmentDescriptor, text: &str, label: &str) -> AttachmentBinding {
    let start = text.find(label).expect("the label stands in the text");
    AttachmentBinding {
        attachment_id: descriptor.id.clone(),
        label: label.to_owned(),
        span: TextSpan::from(start..start + label.len()),
    }
}

/// Enters a Session whose one user Message reads `text` and binds each of
/// `described` to the label `[Image N]` in order.
fn session_with_message(
    application: &mut Application,
    text: &str,
    described: &[AttachmentDescriptor],
) -> SessionSnapshot {
    let workspace = workspace_dir();
    let (_, mut snapshot) = enter_session(application, workspace.path());
    let attachments = described
        .iter()
        .enumerate()
        .map(|(index, descriptor)| bound(descriptor, text, &format!("[Image {}]", index + 1)))
        .collect::<Vec<_>>();
    snapshot.prompts[0].text = text.to_owned();
    snapshot.prompts[0].attachments.clone_from(&attachments);
    let message = snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::User)
        .expect("the fixture carries a user Message");
    message.content = text.to_owned();
    message.attachments = attachments;
    snapshot.attachments = described.to_vec();
    snapshot.revision = SessionRevision(2);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .expect("deliver the Session's snapshot");
    snapshot
}

const TWO: &str = "Compare [Image 1] with [Image 2]";
const FOUR: &str = "All of [Image 1] [Image 2] [Image 3] [Image 4]";

/// One fetch a frame asked for: the request its answer names, and what it
/// asked about.
struct Fetch {
    request: ThumbnailRequest,
    attachment_id: AttachmentId,
    cell_size: CellSize,
}

/// Every fetch the last frame asked for, unanswered, in the order asked.
fn take_fetches(application: &mut Application) -> Vec<Fetch> {
    std::iter::from_fn(|| match application.take_attachment_fetch() {
        ApplicationTransition::FetchAttachment {
            request,
            attachment_id,
            cell_size,
            ..
        } => Some(Fetch {
            request,
            attachment_id,
            cell_size,
        }),
        ApplicationTransition::Continue => None,
        other => panic!("a frame asks only for thumbnails, not {other:?}"),
    })
    .collect()
}

/// Answers `fetch` as the run loop would once the fetch and the decode came
/// back: with a square thumbnail made for the cell size it was asked for, or
/// with the failure of either.
fn answer(application: &mut Application, fetch: &Fetch, succeeded: bool) {
    let event = if succeeded {
        ApplicationEvent::AttachmentThumbnail {
            request: fetch.request,
            attachment_id: fetch.attachment_id.clone(),
            cell_size: fetch.cell_size,
            thumbnail: Thumbnail::from_image(
                DynamicImage::new_rgba8(120, 120),
                fetch.cell_size,
                GraphicsProtocol::Kitty,
            )
            .expect("make a thumbnail from pixels"),
        }
    } else {
        ApplicationEvent::AttachmentThumbnailFailed {
            request: fetch.request,
            attachment_id: fetch.attachment_id.clone(),
            cell_size: fetch.cell_size,
        }
    };
    application.handle_event(event).expect("answer the fetch");
}

/// Answers every thumbnail the last frame asked for with a square one.
/// Answers which Attachments were asked for, in order.
fn answer_fetches(application: &mut Application) -> Vec<AttachmentId> {
    take_fetches(application)
        .into_iter()
        .map(|fetch| {
            answer(application, &fetch, true);
            fetch.attachment_id
        })
        .collect()
}

/// Draws a frame, answers the fetches it asked for, and draws again: the
/// frame a reader sees once the thumbnails arrive.
fn frame_with_thumbnails(application: &mut Application) -> Buffer {
    rendered_application_frame(application, WIDTH, HEIGHT);
    answer_fetches(application);
    rendered_application_frame(application, WIDTH, HEIGHT)
}

/// The column of the first cell on row `y` drawing `symbol`.
fn column_of(buffer: &Buffer, y: u16, symbol: &str) -> Option<u16> {
    (0..buffer.area.width).find(|x| buffer[(*x, y)].symbol() == symbol)
}

/// The row whose cells spell `needle` somewhere, read cell by cell so the
/// rows a thumbnail draws over cannot hide it.
fn row_of(buffer: &Buffer, needle: &str) -> u16 {
    (0..buffer.area.height)
        .find(|y| row_text(buffer, *y).contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is drawn: {:#?}", readable_rows(buffer)))
}

/// A row's text with every thumbnail cell read as `#`, so it can be
/// compared and printed.
fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| {
            let cell = &buffer[(x, y)];
            if cell.symbol().contains(KITTY_PLACEHOLDER) || cell.skip {
                "#"
            } else {
                cell.symbol()
            }
        })
        .collect()
}

fn readable_rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect()
}

/// Each thumbnail row `y` draws: the column its Kitty placeholder stands in
/// and how many columns it spans, counting the skipped cells after it.
fn thumbnails_on(buffer: &Buffer, y: u16) -> Vec<(u16, u16)> {
    let mut thumbnails = Vec::new();
    let mut x = 0;
    while x < buffer.area.width {
        if buffer[(x, y)].symbol().contains(KITTY_PLACEHOLDER) {
            let skipped = (x + 1..buffer.area.width)
                .take_while(|column| buffer[(*column, y)].skip)
                .count();
            let width = u16::try_from(skipped).expect("columns fit") + 1;
            thumbnails.push((x, width));
            x += width;
        } else {
            x += 1;
        }
    }
    thumbnails
}

/// Whether the frame draws no image at all: no placeholder, no skipped cell.
fn draws_no_image(buffer: &Buffer) -> bool {
    buffer
        .content()
        .iter()
        .all(|cell| !cell.skip && !cell.symbol().contains(KITTY_PLACEHOLDER))
}

/// The rows of the user Message block whose first row carries `first`, from
/// that row down to the last that opens with the gutter, each read from the
/// gutter on and trimmed, so nothing beside the block is read with it.
fn message_block(buffer: &Buffer, first: &str) -> (u16, Vec<String>) {
    let start = row_of(buffer, &format!("┃ {first}"));
    let gutter = column_of(buffer, start, "┃").expect("the block opens with its gutter");
    let rows = (start..buffer.area.height)
        .take_while(|y| buffer[(gutter, *y)].symbol() == "┃")
        .map(|y| {
            row_text(buffer, y)
                .chars()
                .skip(usize::from(gutter))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    (start, rows)
}

#[test]
fn a_messages_thumbnails_stand_in_six_rows_beneath_its_text_inside_the_gutter() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);

    rendered_application_frame(&application, WIDTH, HEIGHT);
    assert_eq!(
        answer_fetches(&mut application),
        vec![screenshot().id, diagram().id],
        "the frame asks for each thumbnail it would draw, once"
    );
    assert_eq!(
        answer_fetches(&mut application),
        Vec::<AttachmentId>::new(),
        "nothing is asked for twice"
    );
    let buffer = rendered_application_frame(&application, WIDTH, HEIGHT);

    let (start, block) = message_block(&buffer, TWO);
    assert_eq!(
        block.len(),
        1 + STRIP_ROWS,
        "the text, then six reserved rows: {:#?}",
        readable_rows(&buffer)
    );
    let gutter = column_of(&buffer, start, "┃").expect("the gutter is drawn");
    let left = gutter + 2;
    for y in start + 1..=start + 6 {
        assert_eq!(
            thumbnails_on(&buffer, y),
            vec![
                (left, THUMBNAIL_COLUMNS),
                (left + THUMBNAIL_COLUMNS + 1, THUMBNAIL_COLUMNS)
            ],
            "row {y}: {:#?}",
            readable_rows(&buffer)
        );
        assert_eq!(buffer[(gutter, y)].symbol(), "┃");
    }
    // The dimmed lines have given way to the strip.
    assert!(!readable_rows(&buffer).join("\n").contains("Image 1 ·"));
}

#[test]
fn the_composer_stands_its_thumbnails_in_six_rows_above_its_text() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    type_text(&mut application, "Look at ");
    paste_image(&mut application, screenshot());

    let before = rendered_application_frame(&application, WIDTH, HEIGHT);
    let text = row_of(&before, "Look at [Image 1]");
    assert!(
        row_text(&before, text - 1).contains("Image 1 · PNG · 1280×720 · 312 KiB")
            || (1..=STRIP_ROWS as u16)
                .any(|up| row_text(&before, text - up).contains("Image 1 · PNG")),
        "until its thumbnail arrives, the line stands in the strip's rows: {:#?}",
        readable_rows(&before)
    );

    let buffer = frame_with_thumbnails(&mut application);
    let text = row_of(&buffer, "Look at [Image 1]");
    let border = column_of(&buffer, text, "│").expect("the composer's border is drawn");
    let top = text - STRIP_ROWS as u16;
    assert!(
        row_text(&buffer, top - 1).contains('┌'),
        "the strip is the first thing inside the composer: {:#?}",
        readable_rows(&buffer)
    );
    for y in top..text {
        assert_eq!(
            thumbnails_on(&buffer, y),
            vec![(border + 2, THUMBNAIL_COLUMNS)],
            "row {y}: {:#?}",
            readable_rows(&buffer)
        );
    }
    assert!(row_text(&buffer, text + 1).contains('└'));
}

#[test]
fn four_attachments_show_three_thumbnails_and_a_more_cell() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(
        &mut application,
        FOUR,
        &[screenshot(), diagram(), photo(), sketch()],
    );

    rendered_application_frame(&application, WIDTH, HEIGHT);
    assert_eq!(
        answer_fetches(&mut application),
        vec![screenshot().id, diagram().id, photo().id],
        "only the thumbnails the strip shows are fetched"
    );
    let buffer = rendered_application_frame(&application, WIDTH, HEIGHT);

    let (start, block) = message_block(&buffer, FOUR);
    assert_eq!(block.len(), 1 + STRIP_ROWS, "{:#?}", readable_rows(&buffer));
    let left = column_of(&buffer, start, "┃").expect("the gutter is drawn") + 2;
    let third = left + 2 * (THUMBNAIL_COLUMNS + 1);
    for y in start + 1..=start + 6 {
        assert_eq!(
            thumbnails_on(&buffer, y),
            vec![
                (left, THUMBNAIL_COLUMNS),
                (left + THUMBNAIL_COLUMNS + 1, THUMBNAIL_COLUMNS),
                (third, THUMBNAIL_COLUMNS),
            ],
            "row {y}"
        );
    }
    let more = row_of(&buffer, "+1 more");
    assert!(
        (start + 1..=start + 6).contains(&more),
        "the count stands in the strip: {:#?}",
        readable_rows(&buffer)
    );
    let cell = third + THUMBNAIL_COLUMNS + 1;
    assert_eq!(buffer[(cell, start + 1)].symbol(), "┌", "a bordered cell");
    assert_eq!(buffer[(cell, start + 6)].symbol(), "└");
    let label = row_text(&buffer, more);
    assert!(
        label[..].contains("│ +1 more │"),
        "{label:?} is bordered with air either side"
    );
}

#[test]
fn a_strip_partly_scrolled_off_leaves_its_rows_blank() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    frame_with_thumbnails(&mut application);

    // Shrink the terminal until the Message's text has scrolled off above the
    // Transcript but some of its strip's rows are still drawn.
    let partial = (8..HEIGHT)
        .map(|height| rendered_application_frame(&application, WIDTH, height))
        .find(|buffer| {
            !readable_rows(buffer).join("\n").contains(TWO)
                && (0..buffer.area.height).any(|y| column_of(buffer, y, "┃").is_some())
        })
        .expect("some height scrolls the strip part way off");
    assert!(
        draws_no_image(&partial),
        "a strip partly in view draws nothing: {:#?}",
        readable_rows(&partial)
    );
    let rows = (0..partial.area.height)
        .filter(|y| column_of(&partial, *y, "┃").is_some())
        .map(|y| row_text(&partial, y))
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .all(|row| row.trim_start().trim_start_matches('┃').trim().is_empty()),
        "its visible rows are blank: {rows:#?}"
    );

    // Tall enough again, it is drawn whole.
    let whole = rendered_application_frame(&application, WIDTH, HEIGHT);
    assert!(!draws_no_image(&whole));
}

#[test]
fn a_strip_under_a_picker_or_a_dialog_leaves_its_rows_blank() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    frame_with_thumbnails(&mut application);

    // Drawn tall, the Session picker stands in the middle of the main view,
    // clear of the strip near its top: covering nothing, it leaves the strip
    // drawn.
    invoke(&mut application, SemanticCommandId::SessionList);
    let beside = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, _) = message_block(&beside, TWO);
    assert_eq!(
        thumbnails_on(&beside, start + 1).len(),
        2,
        "a picker clear of the strip covers none of it: {:#?}",
        readable_rows(&beside)
    );
    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);

    // Drawn short, every one of these stands over some of the strip's cells.
    const SHORT: u16 = 20;
    for open in [
        SemanticCommandId::SessionList,
        SemanticCommandId::ThemeList,
        SemanticCommandId::SettingsOpen,
    ] {
        let uncovered = rendered_application_frame(&application, WIDTH, SHORT);
        assert!(
            !draws_no_image(&uncovered),
            "before {open:?}: {:#?}",
            readable_rows(&uncovered)
        );
        invoke(&mut application, open);
        let covered = rendered_application_frame(&application, WIDTH, SHORT);
        assert!(
            draws_no_image(&covered),
            "{open:?} covers the strip: {:#?}",
            readable_rows(&covered)
        );
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    }
    let closed = rendered_application_frame(&application, WIDTH, SHORT);
    assert!(
        !draws_no_image(&closed),
        "closing the last one draws the strip again: {:#?}",
        readable_rows(&closed)
    );
}

#[test]
fn a_strip_beside_the_sidebar_is_still_drawn() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    frame_with_thumbnails(&mut application);

    // The Sidebar takes columns beside the main view rather than covering
    // any of it, so the strip moves over and is still drawn.
    invoke(&mut application, SemanticCommandId::SidebarToggle);
    let beside = rendered_application_frame(&application, 120, HEIGHT);
    let (start, _) = message_block(&beside, TWO);
    let gutter = column_of(&beside, start, "┃").expect("the gutter is drawn");
    assert_eq!(
        thumbnails_on(&beside, start + 1).first().map(|(x, _)| *x),
        Some(gutter + 2),
        "{:#?}",
        readable_rows(&beside)
    );
}

#[test]
fn the_dimmed_lines_stand_in_the_reserved_rows_until_the_thumbnails_arrive() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);

    let waiting = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (_, block) = message_block(&waiting, TWO);
    let block = block
        .iter()
        .map(|row| row.trim().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        block,
        vec![
            format!("┃ {TWO}"),
            "┃ Image 1 · PNG · 1280×720 · 312 KiB".to_owned(),
            "┃ Image 2 · GIF · 320×200 · 312 KiB".to_owned(),
            "┃".to_owned(),
            "┃".to_owned(),
            "┃".to_owned(),
            "┃".to_owned(),
        ],
        "the lines fill the strip's first rows and the rest wait blank"
    );
    assert!(draws_no_image(&waiting));

    // The first to arrive draws the strip: its thumbnail in its slot, and
    // the line of the one still coming in the slot its thumbnail will take —
    // a 320×200 image's, twenty columns — cut to fit.
    let fetches = take_fetches(&mut application);
    answer(&mut application, &fetches[0], true);
    let first = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&first, TWO);
    assert_eq!(block.len(), 1 + STRIP_ROWS, "the layout does not move");
    let left = column_of(&first, start, "┃").expect("the gutter is drawn") + 2;
    assert_eq!(
        thumbnails_on(&first, start + 1),
        vec![(left, THUMBNAIL_COLUMNS)],
        "{:#?}",
        readable_rows(&first)
    );
    assert_eq!(
        block[1],
        format!(
            "┃ {} Image 2 · GIF · 320×",
            "#".repeat(usize::from(THUMBNAIL_COLUMNS))
        )
    );

    answer(&mut application, &fetches[1], true);
    let arrived = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&arrived, TWO);
    assert_eq!(block.len(), 1 + STRIP_ROWS, "the layout does not move");
    assert_eq!(thumbnails_on(&arrived, start + 1).len(), 2);
    assert!(!readable_rows(&arrived).join("\n").contains("Image 1 ·"));
    assert!(!readable_rows(&arrived).join("\n").contains("Image 2 ·"));
}

#[test]
fn a_failed_fetch_leaves_its_line_in_its_slot_beside_the_others_thumbnail() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    rendered_application_frame(&application, WIDTH, HEIGHT);
    let fetches = take_fetches(&mut application);
    answer(&mut application, &fetches[0], false);
    answer(&mut application, &fetches[1], true);

    // The failed 1280×720 image keeps a slot as wide as its thumbnail would
    // have been — twenty-two columns — with its line cut to fit, and the
    // other's thumbnail stands beside it.
    let buffer = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&buffer, TWO);
    assert_eq!(block.len(), 1 + STRIP_ROWS, "{:#?}", readable_rows(&buffer));
    let left = column_of(&buffer, start, "┃").expect("the gutter is drawn") + 2;
    assert!(
        block[1].starts_with("┃ Image 1 · PNG · 1280×7 #"),
        "{block:#?}"
    );
    for y in start + 1..=start + 6 {
        assert_eq!(
            thumbnails_on(&buffer, y),
            vec![(left + 23, THUMBNAIL_COLUMNS)],
            "row {y}: {:#?}",
            readable_rows(&buffer)
        );
    }
    rendered_application_frame(&application, WIDTH, HEIGHT);
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue,
        "a failed fetch is not retried"
    );
}

#[test]
fn when_every_fetch_fails_the_dimmed_lines_stand() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    rendered_application_frame(&application, WIDTH, HEIGHT);
    for fetch in take_fetches(&mut application) {
        answer(&mut application, &fetch, false);
    }

    let buffer = rendered_application_frame(&application, WIDTH, HEIGHT);
    assert!(draws_no_image(&buffer));
    let (_, block) = message_block(&buffer, TWO);
    assert_eq!(block.len(), 1 + STRIP_ROWS);
    assert_eq!(block[1], "┃ Image 1 · PNG · 1280×720 · 312 KiB");
    assert_eq!(block[2], "┃ Image 2 · GIF · 320×200 · 312 KiB");
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue
    );
}

#[test]
fn a_reply_to_a_fetch_since_replaced_never_lands_in_its_place() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    let first = session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    rendered_application_frame(&application, WIDTH, HEIGHT);
    let stale = take_fetches(&mut application);

    // Away to another Session and back, before the first fetches answer: the
    // cache was dropped, and the Attachments are asked for afresh.
    let other = failed_session_snapshot(
        SessionId::new(),
        suru::protocol::PromptId::new(),
        "Another Session",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(other))
        .expect("open another Session");
    rendered_application_frame(&application, WIDTH, HEIGHT);
    application
        .handle_event(ApplicationEvent::SessionAttached(first))
        .expect("return to the first Session");
    rendered_application_frame(&application, WIDTH, HEIGHT);
    let fresh = take_fetches(&mut application);
    assert_eq!(fresh[0].attachment_id, stale[0].attachment_id);

    answer(&mut application, &stale[0], false);
    answer(&mut application, &fresh[0], true);
    answer(&mut application, &stale[1], true);
    let buffer = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&buffer, TWO);
    let left = column_of(&buffer, start, "┃").expect("the gutter is drawn") + 2;
    assert_eq!(
        thumbnails_on(&buffer, start + 1),
        vec![(left, THUMBNAIL_COLUMNS)],
        "the fresh answer stands, and neither stale one lands: {block:#?}"
    );
    assert!(block[1].contains("Image 2 · GIF"), "{block:#?}");
}

/// The block #429 draws: the text, then one dimmed line per Attachment.
fn dimmed_block() -> Vec<String> {
    vec![
        format!("┃ {TWO}"),
        "┃ Image 1 · PNG · 1280×720 · 312 KiB".to_owned(),
        "┃ Image 2 · GIF · 320×200 · 312 KiB".to_owned(),
    ]
}

fn trimmed_block(buffer: &Buffer) -> Vec<String> {
    message_block(buffer, TWO)
        .1
        .iter()
        .map(|row| row.trim().to_owned())
        .collect()
}

#[test]
fn with_the_setting_off_or_no_protocol_the_dimmed_lines_stand_without_reserving_rows() {
    let workspace = workspace_dir();

    let mut off = connected_application_with_terminal_facts(workspace.path(), kitty());
    let mut settings = EffectiveSettings::default();
    settings.transcript.image_previews = false;
    deliver_settings(&mut off, settings);
    session_with_message(&mut off, TWO, &[screenshot(), diagram()]);
    let buffer = rendered_application_frame(&off, WIDTH, HEIGHT);
    assert_eq!(trimmed_block(&buffer), dimmed_block());
    assert!(draws_no_image(&buffer));
    assert_eq!(off.take_attachment_fetch(), ApplicationTransition::Continue);

    let mut unprobed =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::default());
    session_with_message(&mut unprobed, TWO, &[screenshot(), diagram()]);
    let buffer = rendered_application_frame(&unprobed, WIDTH, HEIGHT);
    assert_eq!(trimmed_block(&buffer), dimmed_block());
    assert_eq!(
        unprobed.take_attachment_fetch(),
        ApplicationTransition::Continue
    );

    // Turning the Setting back on reserves the strip again.
    let mut settings = EffectiveSettings::default();
    settings.transcript.image_previews = true;
    deliver_settings(&mut off, settings);
    let buffer = rendered_application_frame(&off, WIDTH, HEIGHT);
    assert_eq!(trimmed_block(&buffer).len(), 1 + STRIP_ROWS);
}

#[test]
fn switching_sessions_drops_the_thumbnails_held() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    let first = session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    let drawn = frame_with_thumbnails(&mut application);
    assert!(!draws_no_image(&drawn));

    let other = failed_session_snapshot(
        SessionId::new(),
        suru::protocol::PromptId::new(),
        "Another Session",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(other))
        .expect("open another Session");
    rendered_application_frame(&application, WIDTH, HEIGHT);
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue
    );

    application
        .handle_event(ApplicationEvent::SessionAttached(first))
        .expect("return to the first Session");
    let returned = rendered_application_frame(&application, WIDTH, HEIGHT);
    assert!(
        draws_no_image(&returned),
        "the thumbnails went with the Session they were held for"
    );
    assert_eq!(
        answer_fetches(&mut application),
        vec![screenshot().id, diagram().id],
        "they are fetched again"
    );
}

#[test]
fn a_drafts_thumbnails_stand_through_the_session_its_prompt_begins() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    type_text(&mut application, "What is ");
    paste_image(&mut application, screenshot());
    frame_with_thumbnails(&mut application);
    let ApplicationTransition::CreateSession(request) =
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("the Landing's Prompt begins a Session");
    };

    // The Provisional Session draws its Prompt with the draft's thumbnail.
    let claimed = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&claimed, "What is [Image 1]");
    assert_eq!(
        block.len(),
        1 + STRIP_ROWS,
        "{:#?}",
        readable_rows(&claimed)
    );
    assert_eq!(thumbnails_on(&claimed, start + 1).len(), 1);
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue
    );

    // The Session answering the claim is the same Session: nothing is
    // fetched again, and the strip stands.
    let mut created = crate::provisional_session::created_session_snapshot(
        SessionId::new(),
        &request.prompt,
        workspace.path(),
        suru::protocol::SessionTimestamp::now(),
    );
    created.attachments = vec![screenshot()];
    application
        .handle_event(ApplicationEvent::SessionCreated(created))
        .expect("take the created Session");
    let arrived = rendered_application_frame(&application, WIDTH, HEIGHT);
    let (start, _) = message_block(&arrived, "What is [Image 1]");
    assert_eq!(
        thumbnails_on(&arrived, start + 1).len(),
        1,
        "{:#?}",
        readable_rows(&arrived)
    );
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue
    );
}

#[test]
fn turning_the_outlook_drops_the_landings_thumbnails() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    type_text(&mut application, "Look at ");
    paste_image(&mut application, screenshot());
    let drawn = frame_with_thumbnails(&mut application);
    assert!(!draws_no_image(&drawn));

    turn_to_studio(&mut application);
    let turned = rendered_application_frame(&application, WIDTH, HEIGHT);
    assert!(
        draws_no_image(&turned),
        "the Landing of another Outlook holds none of this one's thumbnails: {:#?}",
        readable_rows(&turned)
    );
    // The image was pasted to the Server turned away from, so the draft's
    // label stays as text there and no thumbnail is asked of the new one.
    assert_eq!(
        application.take_attachment_fetch(),
        ApplicationTransition::Continue
    );
    let rows = readable_rows(&turned).join("\n");
    assert!(rows.contains("Look at [Image 1]"), "{rows}");
    assert!(rows.contains("Image 1 is on another Server"), "{rows}");
}

#[test]
fn a_thumbnail_goes_with_the_label_deleted_from_the_draft() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    type_text(&mut application, "Look at ");
    paste_image(&mut application, screenshot());
    assert!(!draws_no_image(&frame_with_thumbnails(&mut application)));

    // Backspace takes the space after the label, then the label whole.
    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    rendered_application_frame(&application, WIDTH, HEIGHT);

    // Pasting the very image again finds no thumbnail held for it.
    paste_image(&mut application, screenshot());
    rendered_application_frame(&application, WIDTH, HEIGHT);
    assert_eq!(answer_fetches(&mut application), vec![screenshot().id]);
}

#[test]
fn a_thumbnail_fetch_goes_to_the_open_sessions_origin() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    rendered_application_frame(&application, WIDTH, HEIGHT);
    let ApplicationTransition::FetchAttachment {
        origin,
        attachment_id,
        cell_size,
        protocol,
        ..
    } = application.take_attachment_fetch()
    else {
        panic!("the frame asks for a thumbnail");
    };
    assert_eq!(origin, Outlook::Local);
    assert_eq!(attachment_id, screenshot().id);
    assert_eq!(cell_size, CELL);
    assert_eq!(protocol, GraphicsProtocol::Kitty);
}

#[test]
fn clicking_a_thumbnail_opens_nothing_yet() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(workspace.path(), kitty());
    session_with_message(&mut application, TWO, &[screenshot(), diagram()]);
    let buffer = frame_with_thumbnails(&mut application);
    let (start, _) = message_block(&buffer, TWO);
    let (x, _) = thumbnails_on(&buffer, start + 2)[1];
    let before = buffer_rows(&rendered_application_buffer(&application, WIDTH, HEIGHT));

    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x + 3,
            row: start + 2,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("click the thumbnail");

    assert_eq!(transition, ApplicationTransition::Continue);
    assert_eq!(
        buffer_rows(&rendered_application_buffer(&application, WIDTH, HEIGHT)),
        before,
        "attachment.open does nothing yet"
    );
    assert!(!draws_no_image(&rendered_application_frame(
        &application,
        WIDTH,
        HEIGHT
    )));
}

fn press(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("deliver a key press")
}

/// Turns the Outlook toward the Remote named `studio`, through the Connect
/// picker as a reader does.
fn turn_to_studio(application: &mut Application) {
    invoke(application, SemanticCommandId::ConnectOpen);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            suru::protocol::Remote {
                name: "studio".to_owned(),
                fingerprint: "studio-fingerprint".to_owned(),
                addresses: vec!["10.0.0.8:7777".parse().expect("parse the Remote address")],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(suru::protocol::RemoteHealth {
                protocol_version: Some(suru::protocol::PROTOCOL_VERSION),
                status: suru::protocol::RemoteStatus::Available,
            }),
        })
        .expect("probe the Remote");
    press(application, KeyCode::Down, KeyModifiers::NONE);
    press(application, KeyCode::Enter, KeyModifiers::NONE);
}

fn type_text(application: &mut Application, text: &str) {
    for character in text.chars() {
        press(application, KeyCode::Char(character), KeyModifiers::NONE);
    }
}

/// Pastes an image the whole way — Ctrl+V, the clipboard holding a PNG, and
/// the Server storing it as `stored` — as the run loop would answer each step.
fn paste_image(application: &mut Application, stored: AttachmentDescriptor) {
    let ApplicationTransition::ReadClipboard(paste) =
        press(application, KeyCode::Char('v'), KeyModifiers::CONTROL)
    else {
        panic!("Ctrl+V in the composer reads the clipboard");
    };
    let png = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    let ApplicationTransition::UploadAttachment { .. } = application
        .handle_event(ApplicationEvent::ClipboardRead {
            paste,
            read: ClipboardRead::Image { png },
        })
        .expect("answer the clipboard read")
    else {
        panic!("a clipboard image is uploaded");
    };
    application
        .handle_event(ApplicationEvent::AttachmentUploaded {
            paste,
            descriptor: stored,
        })
        .expect("answer the upload");
}
