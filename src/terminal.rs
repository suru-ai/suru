//! Terminal input and capabilities observed throughout the Application lifetime.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::Stream;

const TERMINAL_COLOR_QUERY_SUFFIX: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
/// Asks whether the terminal speaks the Kitty graphics protocol by querying
/// support for a one-pixel image it never stores.
const KITTY_GRAPHICS_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
/// How every Kitty graphics reply begins.
const KITTY_GRAPHICS_REPLY_PREFIX: &[u8] = b"G";
/// XTVERSION, which asks the terminal for its name and version.
const XTVERSION_QUERY: &[u8] = b"\x1b[>0q";
const XTVERSION_REPLY_PREFIX: &[u8] = b">|";
/// Asks for the size of one cell in pixels, answered as `CSI 6 ; height ;
/// width t`.
const CELL_SIZE_QUERY: &[u8] = b"\x1b[16t";
/// Primary device attributes, which every terminal answers. Sent last, its
/// reply says the terminal has answered everything it is going to, since a
/// terminal answers in the order it was asked.
const PRIMARY_DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
/// The terminals that draw iTerm2 inline images, as XTVERSION names them.
/// iTerm2's protocol has no query, so a terminal's name is all there is to go
/// on.
const ITERM2_IMAGE_TERMINALS: &[&str] = &["iterm2", "wezterm", "vscode", "rio", "mintty"];
/// The same terminals as each names itself in `TERM_PROGRAM`, consulted for
/// iTerm2 inline images alone, since a terminal may draw them without naming
/// itself to XTVERSION.
const ITERM2_IMAGE_PROGRAMS: &[&str] = &["iTerm.app", "WezTerm", "vscode", "rio", "mintty"];
/// The multiplexers under which no image is drawn, as XTVERSION names them.
const MULTIPLEXERS: &[&str] = &["tmux", "screen"];
const DEFAULT_TERMINAL_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// One RGB color reported by the terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl TerminalColor {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// The colors reported by terminal probes.
///
/// Every value is optional because terminals may answer only part of the OSC
/// query. An absent probe is distinct from a partial one and is carried by
/// [`TerminalFacts::probe`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalColorProbe {
    pub palette: [Option<TerminalColor>; 16],
    pub foreground: Option<TerminalColor>,
    pub background: Option<TerminalColor>,
}

impl TerminalColorProbe {
    pub const fn new(
        palette: [Option<TerminalColor>; 16],
        foreground: Option<TerminalColor>,
        background: Option<TerminalColor>,
    ) -> Self {
        Self {
            palette,
            foreground,
            background,
        }
    }
}

/// A terminal graphics protocol an image may be drawn with, in the order Suru
/// prefers them.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GraphicsProtocol {
    /// The Kitty graphics protocol, confirmed by the terminal's own reply to
    /// its query.
    Kitty,
    /// iTerm2 inline images. The protocol has no query, so a terminal speaks
    /// it only where XTVERSION names one known to.
    Iterm2,
    /// Sixel, reported as attribute 4 of the primary device attributes.
    Sixel,
}

impl fmt::Display for GraphicsProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
        })
    }
}

/// The size of one terminal cell in pixels.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl CellSize {
    /// A cell's size, read off a window's size in pixels and in cells, or
    /// `None` where the window reports no pixels, as many do.
    pub(crate) fn from_window(window: ratatui::backend::WindowSize) -> Option<Self> {
        let width = window.pixels.width.checked_div(window.columns_rows.width)?;
        let height = window
            .pixels
            .height
            .checked_div(window.columns_rows.height)?;
        (width > 0 && height > 0).then_some(Self { width, height })
    }
}

/// Why no graphics protocol is selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoGraphics {
    /// Suru runs under tmux or screen, which stand between it and the
    /// terminal that would draw.
    Multiplexed,
    /// The terminal answered for no protocol.
    NoProtocol,
    /// A protocol answered, but nothing said how many pixels a cell covers,
    /// so no image could be sized to the cells it occupies.
    UnknownCellSize,
}

impl fmt::Display for NoGraphics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Multiplexed => "running under tmux or screen",
            Self::NoProtocol => "the terminal answered for no graphics protocol",
            Self::UnknownCellSize => "the cell pixel size is unknown",
        })
    }
}

/// What the terminal and its environment said about drawing images, which is
/// everything graphics selection reads beside the cell size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GraphicsAnswers {
    multiplexed: bool,
    kitty: bool,
    /// Whether the terminal XTVERSION named draws iTerm2 inline images, once
    /// it named one.
    iterm2_named: Option<bool>,
    /// Whether `TERM_PROGRAM` names such a terminal.
    iterm2_program: bool,
    sixel: bool,
    /// The probe has heard all it will: the device attributes arrived, or
    /// the reply window closed first.
    settled: bool,
}

impl GraphicsAnswers {
    const NONE: Self = Self {
        multiplexed: false,
        kitty: false,
        iterm2_named: None,
        iterm2_program: false,
        sixel: false,
        settled: false,
    };
}

/// Everything Suru knows about terminal presentation capabilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFacts {
    pub probe: Option<TerminalColorProbe>,
    pub truecolor: bool,
    /// Whether cells may safely carry OSC 8 hyperlink attributes.
    pub hyperlinks: bool,
    /// The protocol an image may be drawn with, or `None` where none may be:
    /// under tmux or screen, where the terminal answered for no protocol, or
    /// where the cell pixel size is unknown.
    pub graphics: Option<GraphicsProtocol>,
    /// The size of one cell in pixels, where the terminal's window or its
    /// reply said.
    pub cell_size: Option<CellSize>,
    /// What `graphics` is selected from.
    graphics_answers: GraphicsAnswers,
}

impl TerminalFacts {
    pub const fn new(probe: Option<TerminalColorProbe>, truecolor: bool) -> Self {
        Self {
            probe,
            truecolor,
            hyperlinks: false,
            graphics: None,
            cell_size: None,
            graphics_answers: GraphicsAnswers::NONE,
        }
    }

    pub const fn unprobed(truecolor: bool) -> Self {
        Self::new(None, truecolor)
    }

    pub const fn with_hyperlinks(mut self, hyperlinks: bool) -> Self {
        self.hyperlinks = hyperlinks;
        self
    }

    /// Records whether Suru runs under tmux or screen, where no image is
    /// drawn.
    pub fn with_multiplexer(mut self, multiplexed: bool) -> Self {
        self.graphics_answers.multiplexed = multiplexed;
        self.select_graphics();
        self
    }

    /// Records the cell size the terminal's window reports, if it reports
    /// one.
    pub fn with_cell_size(mut self, cell_size: Option<CellSize>) -> Self {
        self.cell_size = cell_size;
        self.select_graphics();
        self
    }

    /// Records the terminal's name as `TERM_PROGRAM` gives it, which counts
    /// only toward iTerm2 inline images: that protocol has no query, so a
    /// terminal known to draw them is taken at its name. It is the fallback
    /// to the name the terminal gives XTVERSION, consulted only once the
    /// probe has settled without an XTVERSION reply, because a terminal
    /// launched from inside another inherits that one's `TERM_PROGRAM` and
    /// must not be taken for it after saying what it is. Until the probe
    /// settles it is not consulted either, so selection never flips when the
    /// reply lands.
    pub fn with_terminal_program(mut self, program: Option<&str>) -> Self {
        self.graphics_answers.iterm2_program = program.is_some_and(|program| {
            ITERM2_IMAGE_PROGRAMS
                .iter()
                .any(|known| program.eq_ignore_ascii_case(known))
        });
        self.select_graphics();
        self
    }

    /// Records that the terminal answered the probe for `protocol`, as its
    /// reply would have, so facts can be built for a protocol without a
    /// terminal to ask. Selection still prefers Kitty, then iTerm2, then
    /// Sixel among everything answered.
    pub fn with_graphics_answer(mut self, protocol: GraphicsProtocol) -> Self {
        let answers = &mut self.graphics_answers;
        match protocol {
            GraphicsProtocol::Kitty => answers.kitty = true,
            GraphicsProtocol::Iterm2 => answers.iterm2_named = Some(true),
            GraphicsProtocol::Sixel => answers.sixel = true,
        }
        self.select_graphics();
        self
    }

    /// Conservatively enables OSC 8 only for terminals whose own identifying
    /// environment is known to support it. Multiplexers are excluded because
    /// an inherited TERM_PROGRAM describes the terminal outside the session,
    /// while passthrough may still be disabled inside it.
    pub(crate) fn hyperlinks_from_environment() -> bool {
        let term_program = std::env::var("TERM_PROGRAM").ok();
        Self::supports_hyperlinks(
            term_program.as_deref(),
            Self::multiplexed_from_environment(),
        )
    }

    /// Whether Suru runs under tmux or screen, as each announces itself to
    /// the processes it hosts.
    pub(crate) fn multiplexed_from_environment() -> bool {
        std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some()
    }

    fn supports_hyperlinks(term_program: Option<&str>, multiplexed: bool) -> bool {
        !multiplexed
            && term_program.is_some_and(|program| {
                matches!(
                    program,
                    "WezTerm" | "iTerm.app" | "vscode" | "ghostty" | "Hyper"
                )
            })
    }

    pub(crate) fn merge_probe(&mut self, update: TerminalColorProbe) {
        let probe = self
            .probe
            .get_or_insert_with(|| TerminalColorProbe::new([None; 16], None, None));
        for (current, update) in probe.palette.iter_mut().zip(update.palette) {
            if update.is_some() {
                *current = update;
            }
        }
        if update.foreground.is_some() {
            probe.foreground = update.foreground;
        }
        if update.background.is_some() {
            probe.background = update.background;
        }
    }

    /// Takes in one answer to the graphics probe and selects afresh from
    /// everything answered so far, so the answers may arrive in any order.
    /// The Log records the selection once, when the probe first settles, and
    /// after that only an answer that changes it.
    pub(crate) fn merge_graphics(&mut self, reply: &GraphicsReply) {
        let before = self.graphics_selection().ok();
        let answers = &mut self.graphics_answers;
        let settles = !answers.settled && reply.settles_probe();
        match reply {
            GraphicsReply::Kitty => answers.kitty = true,
            GraphicsReply::Version(version) => {
                let name = terminal_name(version);
                answers.iterm2_named = Some(ITERM2_IMAGE_TERMINALS.contains(&name.as_str()));
                answers.multiplexed |= MULTIPLEXERS.contains(&name.as_str());
            }
            GraphicsReply::CellSize(cell_size) => self.cell_size = Some(*cell_size),
            GraphicsReply::DeviceAttributes { sixel } => answers.sixel = *sixel,
            GraphicsReply::Expired => {}
        }
        // Settled before selecting, so the reply that settles the probe is
        // the one that lets TERM_PROGRAM stand in for a missing XTVERSION.
        answers.settled |= settles;
        self.select_graphics();
        if settles {
            tracing::info!("{}", self.describe_graphics_selection());
        } else if self.graphics_answers.settled && self.graphics_selection().ok() != before {
            tracing::info!(
                "graphics protocol changed after the probe settled: {}",
                self.describe_graphics_selection()
            );
        }
    }

    /// The protocol an image may be drawn with and the cell size it is drawn
    /// at, or why none may be: Kitty, then iTerm2, then Sixel, but none under
    /// a multiplexer or without a known cell size.
    pub(crate) fn graphics_selection(&self) -> Result<(GraphicsProtocol, CellSize), NoGraphics> {
        let answers = self.graphics_answers;
        if answers.multiplexed {
            return Err(NoGraphics::Multiplexed);
        }
        let protocol = if answers.kitty {
            GraphicsProtocol::Kitty
        } else if answers
            .iterm2_named
            .unwrap_or(answers.settled && answers.iterm2_program)
        {
            GraphicsProtocol::Iterm2
        } else if answers.sixel {
            GraphicsProtocol::Sixel
        } else {
            return Err(NoGraphics::NoProtocol);
        };
        let cell_size = self.cell_size.ok_or(NoGraphics::UnknownCellSize)?;
        Ok((protocol, cell_size))
    }

    fn select_graphics(&mut self) {
        self.graphics = self.graphics_selection().ok().map(|(protocol, _)| protocol);
    }

    /// The graphics selection as the Log records it, for an operator asking
    /// why images are or are not drawn.
    fn describe_graphics_selection(&self) -> String {
        match self.graphics_selection() {
            Ok((protocol, cell_size)) => format!(
                "selected the {protocol} graphics protocol with {}x{} pixel cells",
                cell_size.width, cell_size.height,
            ),
            Err(reason) => format!("selected no graphics protocol: {reason}"),
        }
    }
}

/// The name a terminal gave itself in its XTVERSION reply, lowercased and
/// without the version each terminal spells after it: `iTerm2 3.5.0`,
/// `XTerm(390)`.
fn terminal_name(version: &str) -> String {
    version
        .trim_start()
        .split(|character: char| character.is_whitespace() || character == '(')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

impl Default for TerminalFacts {
    fn default() -> Self {
        Self::unprobed(false)
    }
}

/// One semantic item read from the terminal input stream.
///
/// The parser keeps terminal protocol replies out of the reader-input path, so
/// callers cannot accidentally interpret a reply byte as a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TerminalInput {
    Event(crossterm::event::Event),
    Colors(TerminalColorProbe),
    Graphics(GraphicsReply),
    Reprobe,
}

/// One answer to the startup graphics probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GraphicsReply {
    /// The terminal accepted the Kitty graphics query.
    Kitty,
    /// The name and version XTVERSION reported, as the terminal spelled them.
    Version(String),
    /// The size of one cell in pixels, answering the pixel-size query.
    CellSize(CellSize),
    /// The primary device attributes. Asked for last, so every other answer
    /// the terminal will give has arrived before them.
    DeviceAttributes { sixel: bool },
    /// The response window closed without the device attributes: the
    /// terminal is answering nothing more.
    Expired,
}

impl GraphicsReply {
    /// Whether the probe has nothing more to hear once this arrives.
    pub(crate) fn settles_probe(&self) -> bool {
        matches!(self, Self::DeviceAttributes { .. } | Self::Expired)
    }
}

/// Incrementally turns the terminal's raw byte stream into semantic input.
/// Partial escape sequences remain buffered across calls.
#[derive(Debug, Default)]
pub(crate) struct TerminalInputParser {
    parser: termina::Parser,
    state: RawInputState,
    in_paste: bool,
    expecting_replies: bool,
}

impl TerminalInputParser {
    pub(crate) fn parse(&mut self, bytes: &[u8], maybe_more: bool) -> Vec<TerminalInput> {
        const MAX_CONTROL_STRING_BYTES: usize = 64 * 1024;

        let mut output = Vec::new();
        let mut ordinary = Vec::new();
        for &byte in bytes {
            match &mut self.state {
                RawInputState::Ground if byte == 0x1b => {
                    self.parse_ordinary(&ordinary, true, &mut output);
                    ordinary.clear();
                    self.state = RawInputState::Escape;
                }
                RawInputState::Ground => ordinary.push(byte),
                // Pasted text may carry an escape of its own, and the one
                // after it may begin the end marker, so that one is read
                // afresh.
                RawInputState::Escape if self.in_paste && byte == 0x1b => {
                    ordinary.push(0x1b);
                    self.parse_ordinary(&ordinary, true, &mut output);
                    ordinary.clear();
                }
                RawInputState::Escape => {
                    self.state = match ControlString::introduced_by(byte) {
                        Some(kind) if !self.in_paste => RawInputState::ControlString {
                            kind,
                            payload: Vec::new(),
                            escape_terminator: false,
                        },
                        _ if byte == b'[' => RawInputState::Csi {
                            raw: b"\x1b[".to_vec(),
                        },
                        _ => {
                            ordinary.extend_from_slice(&[0x1b, byte]);
                            RawInputState::Ground
                        }
                    };
                }
                // A CSI never carries an escape, so one arriving mid-sequence
                // cuts it short and begins the next: what came before is
                // released as an idle boundary would release it, and the
                // escape is read afresh.
                RawInputState::Csi { raw } if byte == 0x1b => {
                    release_csi(raw, self.in_paste, &mut ordinary, &mut output);
                    self.parse_ordinary(&ordinary, true, &mut output);
                    ordinary.clear();
                    self.state = RawInputState::Escape;
                }
                RawInputState::Csi { raw } => {
                    raw.push(byte);
                    if raw == BRACKETED_PASTE_START || raw == BRACKETED_PASTE_END {
                        self.in_paste = raw == BRACKETED_PASTE_START;
                        ordinary.append(raw);
                        self.state = RawInputState::Ground;
                    } else {
                        // Pasted text is the reader's, however much of it looks
                        // like a reply.
                        let reply = if self.in_paste {
                            CsiReply::Other
                        } else {
                            csi_reply(raw)
                        };
                        match reply {
                            CsiReply::Complete(input) => {
                                output.extend(input);
                                self.state = RawInputState::Ground;
                            }
                            CsiReply::Partial => {}
                            CsiReply::Other
                                if BRACKETED_PASTE_START.starts_with(raw)
                                    || BRACKETED_PASTE_END.starts_with(raw) => {}
                            CsiReply::Other => {
                                ordinary.append(raw);
                                self.state = RawInputState::Ground;
                            }
                        }
                    }
                }
                RawInputState::ControlString {
                    kind,
                    payload,
                    escape_terminator,
                } => {
                    let kind = *kind;
                    // Only a string whose reply prefix matched is read to its
                    // terminator and swallowed. Before that, a terminator is a
                    // key like any other that begins no reply, and an escape
                    // begins the reader's next key.
                    let recognized = kind.is_confirmed_reply(payload);
                    if *escape_terminator {
                        if byte == b'\\' {
                            output.extend(kind.reply(payload));
                            self.state = RawInputState::Ground;
                        } else {
                            payload.extend_from_slice(&[0x1b, byte]);
                            *escape_terminator = false;
                        }
                    } else if recognized && byte == 0x07 {
                        output.extend(kind.reply(payload));
                        self.state = RawInputState::Ground;
                    } else if recognized && byte == 0x1b {
                        *escape_terminator = true;
                    } else if byte == 0x1b {
                        output.push(kind.alt_chord());
                        ordinary.extend_from_slice(payload);
                        self.parse_ordinary(&ordinary, true, &mut output);
                        ordinary.clear();
                        self.state = RawInputState::Escape;
                    } else {
                        payload.push(byte);
                        if payload.len() > MAX_CONTROL_STRING_BYTES {
                            self.state = RawInputState::DiscardControlString {
                                escape_terminator: false,
                            };
                        } else if !kind.could_be_reply(payload) {
                            output.push(kind.alt_chord());
                            ordinary.extend_from_slice(payload);
                            self.state = RawInputState::Ground;
                        }
                    }
                }
                RawInputState::DiscardControlString { escape_terminator } => {
                    if *escape_terminator && byte == b'\\' || byte == 0x07 {
                        self.state = RawInputState::Ground;
                    } else {
                        *escape_terminator = byte == 0x1b;
                    }
                }
            }
        }
        if !maybe_more {
            // Inside a paste every byte is the paste's until its end marker,
            // so a marker split at the boundary waits for the rest of itself
            // and nothing in it is ever taken for a key.
            let paste_end_pending =
                |raw: &[u8]| self.in_paste && BRACKETED_PASTE_END.starts_with(raw);
            match &mut self.state {
                RawInputState::Escape if !paste_end_pending(b"\x1b") => {
                    ordinary.push(0x1b);
                    self.state = RawInputState::Ground;
                }
                // A reply the probe awaits survives the boundary until it can
                // no longer be one or the reply window closes.
                RawInputState::Csi { raw }
                    if !paste_end_pending(raw)
                        && !(self.expecting_replies && is_awaited_csi_reply(raw)) =>
                {
                    release_csi(raw, self.in_paste, &mut ordinary, &mut output);
                    self.state = RawInputState::Ground;
                }
                // Only an ambiguous Alt chord expires. Once a reply's prefix is
                // recognized, read through its terminator even if it is late.
                RawInputState::ControlString { kind, payload, .. }
                    if !self.expecting_replies && !kind.is_confirmed_reply(payload) =>
                {
                    output.push(kind.alt_chord());
                    ordinary.extend_from_slice(payload);
                    self.state = RawInputState::Ground;
                }
                RawInputState::Ground
                | RawInputState::Escape
                | RawInputState::Csi { .. }
                | RawInputState::ControlString { .. }
                | RawInputState::DiscardControlString { .. } => {}
            }
        }
        self.parse_ordinary(&ordinary, maybe_more, &mut output);
        output
    }

    fn parse_ordinary(&mut self, bytes: &[u8], maybe_more: bool, output: &mut Vec<TerminalInput>) {
        self.parser.parse(bytes, maybe_more);
        output.extend(
            std::iter::from_fn(|| self.parser.pop()).filter_map(terminal_input_from_termina),
        );
    }

    fn expect_replies(&mut self) {
        self.expecting_replies = true;
    }

    fn stop_expecting_replies(&mut self) {
        // The query window only governs ambiguous prefixes. A recognized
        // reply may still finish later and must retain what it accumulated.
        self.expecting_replies = false;
    }
}

/// The control strings a terminal answers Suru's queries with. Each opens
/// with an escape the reader's own Alt chord also sends, so a string that
/// turns out to begin no reply is handed back as that chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlString {
    /// OSC, which carries the color replies.
    Osc,
    /// DCS, which carries the XTVERSION reply.
    Dcs,
    /// APC, which carries the Kitty graphics reply.
    Apc,
}

impl ControlString {
    fn introduced_by(byte: u8) -> Option<Self> {
        match byte {
            b']' => Some(Self::Osc),
            b'P' => Some(Self::Dcs),
            b'_' => Some(Self::Apc),
            _ => None,
        }
    }

    /// The Alt chord the reader pressed, when the bytes begin no reply.
    fn alt_chord(self) -> TerminalInput {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let (character, modifiers) = match self {
            Self::Osc => (']', KeyModifiers::ALT),
            Self::Dcs => ('P', KeyModifiers::ALT | KeyModifiers::SHIFT),
            Self::Apc => ('_', KeyModifiers::ALT),
        };
        TerminalInput::Event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            modifiers,
        )))
    }

    fn reply_prefixes(self) -> &'static [&'static [u8]] {
        match self {
            Self::Osc => &[b"4;", b"10;", b"11;"],
            Self::Dcs => &[XTVERSION_REPLY_PREFIX],
            Self::Apc => &[KITTY_GRAPHICS_REPLY_PREFIX],
        }
    }

    /// Whether `payload` may still grow into a reply to one of Suru's queries.
    fn could_be_reply(self, payload: &[u8]) -> bool {
        self.reply_prefixes()
            .iter()
            .any(|prefix| prefix.starts_with(payload) || payload.starts_with(prefix))
    }

    /// Whether `payload` is certainly a reply, however late its terminator.
    fn is_confirmed_reply(self, payload: &[u8]) -> bool {
        self.reply_prefixes()
            .iter()
            .any(|prefix| payload.starts_with(prefix))
    }

    /// What a complete string says, if it is a reply Suru understands. Any
    /// other string is swallowed, since a reply is never the reader's input.
    fn reply(self, payload: &[u8]) -> Option<TerminalInput> {
        match self {
            Self::Osc => std::str::from_utf8(payload)
                .ok()
                .and_then(parse_color_reply)
                .map(TerminalInput::Colors),
            Self::Dcs => payload.strip_prefix(XTVERSION_REPLY_PREFIX).map(|version| {
                TerminalInput::Graphics(GraphicsReply::Version(
                    String::from_utf8_lossy(version).trim().to_owned(),
                ))
            }),
            Self::Apc => parse_kitty_graphics_reply(payload),
        }
    }
}

/// A Kitty graphics reply, `G<keys>;<message>`, confirms the protocol only
/// when it answers the probe's image id with `OK`: an error message says the
/// terminal parsed the query but will not draw.
fn parse_kitty_graphics_reply(payload: &[u8]) -> Option<TerminalInput> {
    let reply = payload.strip_prefix(b"G")?;
    let separator = reply.iter().position(|byte| *byte == b';')?;
    let (keys, message) = (&reply[..separator], &reply[separator + 1..]);
    (keys.split(|byte| *byte == b',').any(|key| key == b"i=31") && message == b"OK")
        .then_some(TerminalInput::Graphics(GraphicsReply::Kitty))
}

/// Where the CSI bytes read so far stand against the CSI replies Suru asks
/// for.
enum CsiReply {
    /// Still a prefix of a reply.
    Partial,
    /// A whole reply, carrying what it says if Suru understands it.
    Complete(Option<TerminalInput>),
    /// No reply, so the bytes are the reader's input.
    Other,
}

/// Hands a CSI that will not complete to termina as reader input. termina
/// holds a bare introducer for more that is not coming, so outside a paste
/// that one is resolved here as the chord that sent it; inside a paste it is
/// pasted text like any other.
fn release_csi(
    raw: &mut Vec<u8>,
    in_paste: bool,
    ordinary: &mut Vec<u8>,
    output: &mut Vec<TerminalInput>,
) {
    if raw == b"\x1b[" && !in_paste {
        raw.clear();
        output.push(TerminalInput::Event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('['),
                crossterm::event::KeyModifiers::ALT,
            ),
        )));
    } else {
        ordinary.append(raw);
    }
}

/// Whether `raw` may still become the device attributes or the cell size,
/// the CSI replies the probe awaits: from the bare `ESC [` on, before either
/// has identified itself. Only the parser's CSI state holds `raw`, and it
/// holds nothing but a prefix of a reply or of a bracketed paste marker.
fn is_awaited_csi_reply(raw: &[u8]) -> bool {
    [b"\x1b[?".as_slice(), b"\x1b[6;".as_slice()]
        .iter()
        .any(|prefix| prefix.starts_with(raw) || raw.starts_with(prefix))
}

/// Recognizes the primary device attributes, `CSI ? Ps ; … c`, and the cell
/// size, `CSI 6 ; height ; width t`, in `raw`, which begins `ESC [`.
fn csi_reply(raw: &[u8]) -> CsiReply {
    fn is_parameters(bytes: &[u8]) -> bool {
        bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
    }

    let body = &raw[2..];
    if let Some(parameters) = body.strip_prefix(b"?") {
        return match parameters.split_last() {
            Some((b'c', parameters)) if is_parameters(parameters) => {
                // The first parameter is the terminal's class; the attributes
                // follow it, and attribute 4 is Sixel.
                let sixel = parameters
                    .split(|byte| *byte == b';')
                    .skip(1)
                    .any(|attribute| attribute == b"4");
                CsiReply::Complete(Some(TerminalInput::Graphics(
                    GraphicsReply::DeviceAttributes { sixel },
                )))
            }
            _ if is_parameters(parameters) => CsiReply::Partial,
            _ => CsiReply::Other,
        };
    }
    if b"6;".starts_with(body) {
        return CsiReply::Partial;
    }
    let Some(sizes) = body.strip_prefix(b"6;") else {
        return CsiReply::Other;
    };
    match sizes.split_last() {
        Some((b't', sizes)) if is_parameters(sizes) => {
            let size = std::str::from_utf8(sizes).ok().and_then(|sizes| {
                let (height, width) = sizes.split_once(';')?;
                let size = CellSize {
                    width: width.parse().ok()?,
                    height: height.parse().ok()?,
                };
                (size.width > 0 && size.height > 0).then_some(size)
            });
            CsiReply::Complete(
                size.map(|size| TerminalInput::Graphics(GraphicsReply::CellSize(size))),
            )
        }
        _ if is_parameters(sizes) && sizes.iter().filter(|byte| **byte == b';').count() <= 1 => {
            CsiReply::Partial
        }
        _ => CsiReply::Other,
    }
}

#[derive(Debug, Default)]
enum RawInputState {
    #[default]
    Ground,
    Escape,
    Csi {
        raw: Vec<u8>,
    },
    ControlString {
        kind: ControlString,
        payload: Vec<u8>,
        escape_terminator: bool,
    },
    DiscardControlString {
        escape_terminator: bool,
    },
}

fn parse_color_reply(payload: &str) -> Option<TerminalColorProbe> {
    let mut probe = TerminalColorProbe::new([None; 16], None, None);
    apply_osc_color(payload, &mut probe, &mut SeenTerminalReplies::default()).then_some(probe)
}

fn terminal_input_from_termina(event: termina::Event) -> Option<TerminalInput> {
    use termina::event::{MouseButton, MouseEventKind};

    let event = match event {
        termina::Event::Key(key) => {
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind_and_state(
                key_code_from_termina(key.code),
                key_modifiers_from_termina(key.modifiers),
                match key.kind {
                    termina::event::KeyEventKind::Press => crossterm::event::KeyEventKind::Press,
                    termina::event::KeyEventKind::Repeat => crossterm::event::KeyEventKind::Repeat,
                    termina::event::KeyEventKind::Release => {
                        crossterm::event::KeyEventKind::Release
                    }
                },
                key_state_from_termina(key.state, key.modifiers),
            ))
        }
        termina::Event::Mouse(mouse) => {
            let button = |button| match button {
                MouseButton::Left => crossterm::event::MouseButton::Left,
                MouseButton::Right => crossterm::event::MouseButton::Right,
                MouseButton::Middle => crossterm::event::MouseButton::Middle,
            };
            let kind = match mouse.kind {
                MouseEventKind::Down(value) => {
                    crossterm::event::MouseEventKind::Down(button(value))
                }
                MouseEventKind::Up(value) => crossterm::event::MouseEventKind::Up(button(value)),
                MouseEventKind::Drag(value) => {
                    crossterm::event::MouseEventKind::Drag(button(value))
                }
                MouseEventKind::Moved => crossterm::event::MouseEventKind::Moved,
                MouseEventKind::ScrollDown => crossterm::event::MouseEventKind::ScrollDown,
                MouseEventKind::ScrollUp => crossterm::event::MouseEventKind::ScrollUp,
                MouseEventKind::ScrollLeft => crossterm::event::MouseEventKind::ScrollLeft,
                MouseEventKind::ScrollRight => crossterm::event::MouseEventKind::ScrollRight,
            };
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: mouse.column,
                row: mouse.row,
                modifiers: key_modifiers_from_termina(mouse.modifiers),
            })
        }
        termina::Event::Paste(text) => crossterm::event::Event::Paste(text),
        termina::Event::WindowResized(size) => {
            crossterm::event::Event::Resize(size.cols, size.rows)
        }
        termina::Event::FocusIn => crossterm::event::Event::FocusGained,
        termina::Event::FocusOut => crossterm::event::Event::FocusLost,
        termina::Event::Csi(termina::escape::csi::Csi::Mode(
            termina::escape::csi::Mode::ReportTheme(_),
        )) => return Some(TerminalInput::Reprobe),
        _ => return None,
    };
    Some(TerminalInput::Event(event))
}

fn key_code_from_termina(code: termina::event::KeyCode) -> crossterm::event::KeyCode {
    use crossterm::event::KeyCode as To;
    use termina::event::KeyCode as From;

    match code {
        From::Backspace => To::Backspace,
        From::Enter => To::Enter,
        From::Left => To::Left,
        From::Right => To::Right,
        From::Up => To::Up,
        From::Down => To::Down,
        From::Home => To::Home,
        From::End => To::End,
        From::PageUp => To::PageUp,
        From::PageDown => To::PageDown,
        From::Tab => To::Tab,
        From::BackTab => To::BackTab,
        From::Delete => To::Delete,
        From::Insert => To::Insert,
        From::Function(number) => To::F(number),
        From::Char(character) => To::Char(character),
        From::Null => To::Null,
        From::Escape => To::Esc,
        From::CapsLock => To::CapsLock,
        From::ScrollLock => To::ScrollLock,
        From::NumLock => To::NumLock,
        From::PrintScreen => To::PrintScreen,
        From::Pause => To::Pause,
        From::Menu => To::Menu,
        From::KeypadBegin => To::KeypadBegin,
        From::Media(media) => To::Media(media_key_from_termina(media)),
        From::Modifier(modifier) => To::Modifier(modifier_key_from_termina(modifier)),
    }
}

fn media_key_from_termina(code: termina::event::MediaKeyCode) -> crossterm::event::MediaKeyCode {
    use crossterm::event::MediaKeyCode as To;
    use termina::event::MediaKeyCode as From;

    match code {
        From::Play => To::Play,
        From::Pause => To::Pause,
        From::PlayPause => To::PlayPause,
        From::Reverse => To::Reverse,
        From::Stop => To::Stop,
        From::FastForward => To::FastForward,
        From::Rewind => To::Rewind,
        From::TrackNext => To::TrackNext,
        From::TrackPrevious => To::TrackPrevious,
        From::Record => To::Record,
        From::LowerVolume => To::LowerVolume,
        From::RaiseVolume => To::RaiseVolume,
        From::MuteVolume => To::MuteVolume,
    }
}

fn modifier_key_from_termina(
    code: termina::event::ModifierKeyCode,
) -> crossterm::event::ModifierKeyCode {
    use crossterm::event::ModifierKeyCode as To;
    use termina::event::ModifierKeyCode as From;

    match code {
        From::LeftShift => To::LeftShift,
        From::LeftControl => To::LeftControl,
        From::LeftAlt => To::LeftAlt,
        From::LeftSuper => To::LeftSuper,
        From::LeftHyper => To::LeftHyper,
        From::LeftMeta => To::LeftMeta,
        From::RightShift => To::RightShift,
        From::RightControl => To::RightControl,
        From::RightAlt => To::RightAlt,
        From::RightSuper => To::RightSuper,
        From::RightHyper => To::RightHyper,
        From::RightMeta => To::RightMeta,
        From::IsoLevel3Shift => To::IsoLevel3Shift,
        From::IsoLevel5Shift => To::IsoLevel5Shift,
    }
}

fn key_modifiers_from_termina(
    modifiers: termina::event::Modifiers,
) -> crossterm::event::KeyModifiers {
    let mut result = crossterm::event::KeyModifiers::NONE;
    for (source, target) in [
        (
            termina::event::Modifiers::SHIFT,
            crossterm::event::KeyModifiers::SHIFT,
        ),
        (
            termina::event::Modifiers::ALT,
            crossterm::event::KeyModifiers::ALT,
        ),
        (
            termina::event::Modifiers::CONTROL,
            crossterm::event::KeyModifiers::CONTROL,
        ),
        (
            termina::event::Modifiers::SUPER,
            crossterm::event::KeyModifiers::SUPER,
        ),
        (
            termina::event::Modifiers::HYPER,
            crossterm::event::KeyModifiers::HYPER,
        ),
        (
            termina::event::Modifiers::META,
            crossterm::event::KeyModifiers::META,
        ),
    ] {
        if modifiers.contains(source) {
            result.insert(target);
        }
    }
    result
}

fn key_state_from_termina(
    state: termina::event::KeyEventState,
    modifiers: termina::event::Modifiers,
) -> crossterm::event::KeyEventState {
    let mut result = crossterm::event::KeyEventState::NONE;
    for (present, target) in [
        (
            state.contains(termina::event::KeyEventState::KEYPAD),
            crossterm::event::KeyEventState::KEYPAD,
        ),
        (
            state.contains(termina::event::KeyEventState::CAPS_LOCK)
                || modifiers.contains(termina::event::Modifiers::CAPS_LOCK),
            crossterm::event::KeyEventState::CAPS_LOCK,
        ),
        (
            state.contains(termina::event::KeyEventState::NUM_LOCK)
                || modifiers.contains(termina::event::Modifiers::NUM_LOCK),
            crossterm::event::KeyEventState::NUM_LOCK,
        ),
    ] {
        if present {
            result.insert(target);
        }
    }
    result
}

#[derive(Debug)]
enum TerminalSourceInput {
    Bytes(Vec<u8>),
    #[cfg(windows)]
    Event(crossterm::event::Event),
    Error(io::Error),
}

/// The process terminal's single input owner.
///
/// Platform adapters deliver raw bytes (and native resize notifications) to
/// the same parser, so protocol replies and reader input cannot race through
/// independent consumers.
pub(crate) struct TerminalEvents {
    _reader: Option<TerminalReaderGuard>,
    source: tokio::sync::mpsc::UnboundedReceiver<TerminalSourceInput>,
    parser: TerminalInputParser,
    ready: VecDeque<io::Result<TerminalInput>>,
    source_done: bool,
    sequence_timeout: Duration,
    sequence_idle: Option<Pin<Box<tokio::time::Sleep>>>,
    reply_timeout: Duration,
    /// When the terminal is taken to have answered every outstanding query.
    reply_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Whether the graphics probe has yet to settle, either with the device
    /// attributes it ends with or with the reply deadline passing.
    awaiting_graphics: bool,
    #[cfg(unix)]
    resize: tokio::signal::unix::Signal,
}

impl TerminalEvents {
    pub(crate) fn open() -> io::Result<Self> {
        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let reader = spawn_terminal_reader(sender)?;
        Self::from_source_with_reader(
            source,
            Some(reader),
            DEFAULT_TERMINAL_SEQUENCE_TIMEOUT,
            DEFAULT_REPLY_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn from_source(
        source: tokio::sync::mpsc::UnboundedReceiver<TerminalSourceInput>,
    ) -> io::Result<Self> {
        Self::from_source_with_reader(
            source,
            None,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
    }

    fn from_source_with_reader(
        source: tokio::sync::mpsc::UnboundedReceiver<TerminalSourceInput>,
        reader: Option<TerminalReaderGuard>,
        sequence_timeout: Duration,
        reply_timeout: Duration,
    ) -> io::Result<Self> {
        Ok(Self {
            _reader: reader,
            source,
            parser: TerminalInputParser::default(),
            ready: VecDeque::new(),
            source_done: false,
            sequence_timeout,
            sequence_idle: None,
            reply_timeout,
            reply_deadline: None,
            awaiting_graphics: false,
            #[cfg(unix)]
            resize: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?,
        })
    }

    /// Issue the startup probe without consuming input or delaying the first
    /// frame. Replies and reader input stay ordered in the normal event stream.
    pub(crate) fn request_probe(
        &mut self,
        output: &mut impl io::Write,
        facts: &TerminalFacts,
    ) -> io::Result<()> {
        self.expect_replies();
        self.awaiting_graphics = true;
        request_terminal_probe(output, facts)
    }

    fn expect_replies(&mut self) {
        self.parser.expect_replies();
        self.reply_deadline = Some(Box::pin(tokio::time::sleep(self.reply_timeout)));
    }

    fn queue_parsed(&mut self, parsed: Vec<TerminalInput>) {
        let refresh_expectation = parsed.iter().any(|input| {
            matches!(input, TerminalInput::Reprobe)
                || (self.reply_deadline.is_some() && matches!(input, TerminalInput::Colors(_)))
        });
        if parsed.iter().any(|input| {
            matches!(
                input,
                TerminalInput::Graphics(GraphicsReply::DeviceAttributes { .. })
            )
        }) {
            self.awaiting_graphics = false;
        }
        self.ready.extend(parsed.into_iter().map(Ok));
        if refresh_expectation {
            self.expect_replies();
        }
    }
}

/// Writes the startup probe as one burst: the color queries, then the
/// graphics queries whose answers the facts do not already hold, ending with
/// the primary device attributes, which every terminal answers. A terminal
/// that ignores everything else still answers those, so the probe settles;
/// under a multiplexer they are all that is asked, since no image will be
/// drawn there and screen would take a Kitty query for a status line.
pub(crate) fn request_terminal_probe(
    output: &mut impl io::Write,
    facts: &TerminalFacts,
) -> io::Result<()> {
    write_color_queries(output)?;
    if !facts.graphics_answers.multiplexed {
        output.write_all(KITTY_GRAPHICS_QUERY)?;
        output.write_all(XTVERSION_QUERY)?;
        if facts.cell_size.is_none() {
            output.write_all(CELL_SIZE_QUERY)?;
        }
    }
    output.write_all(PRIMARY_DEVICE_ATTRIBUTES_QUERY)?;
    output.flush()
}

pub(crate) fn request_terminal_colors(output: &mut impl io::Write) -> io::Result<()> {
    write_color_queries(output)?;
    output.flush()
}

fn write_color_queries(output: &mut impl io::Write) -> io::Result<()> {
    for index in 0..16 {
        write!(output, "\x1b]4;{index};?\x1b\\")?;
    }
    output.write_all(TERMINAL_COLOR_QUERY_SUFFIX)
}

impl Stream for TerminalEvents {
    type Item = io::Result<TerminalInput>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.ready.pop_front() {
                return Poll::Ready(Some(event));
            }

            match this.source.poll_recv(context) {
                Poll::Ready(Some(TerminalSourceInput::Bytes(bytes))) => {
                    let parsed = this.parser.parse(&bytes, true);
                    this.queue_parsed(parsed);
                    // A read boundary is not a protocol boundary. Only settle an
                    // ambiguous standalone Escape after the terminal stays idle.
                    this.sequence_idle = Some(Box::pin(tokio::time::sleep(this.sequence_timeout)));
                    continue;
                }
                #[cfg(windows)]
                Poll::Ready(Some(TerminalSourceInput::Event(event))) => {
                    return Poll::Ready(Some(Ok(TerminalInput::Event(event))));
                }
                Poll::Ready(Some(TerminalSourceInput::Error(error))) => {
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) if !this.source_done => {
                    this.source_done = true;
                    this.sequence_idle = None;
                    this.reply_deadline = None;
                    this.parser.stop_expecting_replies();
                    let parsed = this.parser.parse(&[], false);
                    this.queue_parsed(parsed);
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }

            if this
                .sequence_idle
                .as_mut()
                .is_some_and(|timeout| timeout.as_mut().poll(context).is_ready())
            {
                this.sequence_idle = None;
                let parsed = this.parser.parse(&[], false);
                this.queue_parsed(parsed);
                continue;
            }

            if this
                .reply_deadline
                .as_mut()
                .is_some_and(|timeout| timeout.as_mut().poll(context).is_ready())
            {
                this.reply_deadline = None;
                this.parser.stop_expecting_replies();
                let parsed = this.parser.parse(&[], false);
                this.queue_parsed(parsed);
                if std::mem::take(&mut this.awaiting_graphics) {
                    this.ready
                        .push_back(Ok(TerminalInput::Graphics(GraphicsReply::Expired)));
                }
                continue;
            }

            #[cfg(unix)]
            match this.resize.poll_recv(context) {
                Poll::Ready(Some(())) => match crossterm::terminal::size() {
                    Ok((columns, rows)) => {
                        return Poll::Ready(Some(Ok(TerminalInput::Event(
                            crossterm::event::Event::Resize(columns, rows),
                        ))));
                    }
                    Err(error) => return Poll::Ready(Some(Err(error))),
                },
                Poll::Ready(None) | Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}

/// Wakes and joins the platform reader before terminal modes are restored.
struct TerminalReaderGuard {
    #[cfg(unix)]
    wake: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    wake: std::sync::Arc<std::os::windows::io::OwnedHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TerminalReaderGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::io::Write as _;

            let _ = self.wake.write_all(&[0]);
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle as _;

            windows_sys::Win32::System::Threading::SetEvent(self.wake.as_raw_handle());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
fn spawn_terminal_reader(
    sender: tokio::sync::mpsc::UnboundedSender<TerminalSourceInput>,
) -> io::Result<TerminalReaderGuard> {
    use std::{
        fs::File,
        io::{IsTerminal as _, Read as _},
        os::{
            fd::{AsRawFd as _, FromRawFd as _},
            unix::net::UnixStream,
        },
    };

    let mut input = if io::stdin().is_terminal() {
        let descriptor = unsafe { libc::dup(libc::STDIN_FILENO) };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        unsafe { File::from_raw_fd(descriptor) }
    } else {
        File::options().read(true).write(true).open("/dev/tty")?
    };
    let (wake_read, wake) = UnixStream::pair()?;
    let thread = std::thread::Builder::new()
        .name("suru-terminal-input".to_owned())
        .spawn(move || {
            let mut buffer = [0; 1024];
            loop {
                let mut ready: libc::fd_set = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::FD_ZERO(&mut ready);
                    libc::FD_SET(input.as_raw_fd(), &mut ready);
                    libc::FD_SET(wake_read.as_raw_fd(), &mut ready);
                }
                let result = unsafe {
                    libc::select(
                        input.as_raw_fd().max(wake_read.as_raw_fd()) + 1,
                        &mut ready,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if result == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    let _ = sender.send(TerminalSourceInput::Error(error));
                    break;
                }
                if unsafe { libc::FD_ISSET(wake_read.as_raw_fd(), &ready) } {
                    break;
                }
                match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender
                            .send(TerminalSourceInput::Bytes(buffer[..count].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = sender.send(TerminalSourceInput::Error(error));
                        break;
                    }
                }
            }
        })?;
    Ok(TerminalReaderGuard {
        wake,
        thread: Some(thread),
    })
}

#[cfg(windows)]
fn spawn_terminal_reader(
    sender: tokio::sync::mpsc::UnboundedSender<TerminalSourceInput>,
) -> io::Result<TerminalReaderGuard> {
    use std::{
        fs::File,
        mem,
        os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
        ptr,
        sync::Arc,
    };
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0},
        System::{
            Console::{INPUT_RECORD, ReadConsoleInputA},
            Threading::{CreateEventW, WaitForMultipleObjects},
        },
    };

    let input = File::options().read(true).write(true).open("CONIN$")?;
    let wake = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if wake.is_null() {
        return Err(io::Error::last_os_error());
    }
    let wake = Arc::new(unsafe { OwnedHandle::from_raw_handle(wake) });
    let thread_wake = Arc::clone(&wake);
    let thread = std::thread::Builder::new()
        .name("suru-terminal-input".to_owned())
        .spawn(move || {
            let mut records: [INPUT_RECORD; 128] = unsafe { mem::zeroed() };
            loop {
                let mut handles = [thread_wake.as_raw_handle(), input.as_raw_handle()];
                let ready = unsafe {
                    WaitForMultipleObjects(handles.len() as u32, handles.as_mut_ptr(), 0, u32::MAX)
                };
                if ready == WAIT_OBJECT_0 {
                    break;
                }
                if ready == WAIT_FAILED {
                    let _ = sender.send(TerminalSourceInput::Error(io::Error::last_os_error()));
                    break;
                }
                if ready != WAIT_OBJECT_0 + 1 {
                    continue;
                }
                let mut count = 0;
                if unsafe {
                    ReadConsoleInputA(
                        input.as_raw_handle(),
                        records.as_mut_ptr(),
                        records.len() as u32,
                        &mut count,
                    )
                } == 0
                {
                    let _ = sender.send(TerminalSourceInput::Error(io::Error::last_os_error()));
                    break;
                }
                for item in decode_windows_input_records(&records[..count as usize]) {
                    if sender.send(item).is_err() {
                        return;
                    }
                }
            }
        })?;
    Ok(TerminalReaderGuard {
        wake,
        thread: Some(thread),
    })
}

#[cfg(windows)]
fn decode_windows_input_records(
    records: &[windows_sys::Win32::System::Console::INPUT_RECORD],
) -> Vec<TerminalSourceInput> {
    use std::mem;
    use windows_sys::Win32::System::Console::{KEY_EVENT, WINDOW_BUFFER_SIZE_EVENT};

    let mut output = Vec::new();
    let mut bytes = Vec::new();
    for record in records {
        match record.EventType as u32 {
            KEY_EVENT => {
                let record = unsafe { record.Event.KeyEvent };
                if record.bKeyDown == 0 {
                    continue;
                }
                let byte = unsafe { record.uChar.AsciiChar } as u8;
                if byte != 0 {
                    bytes.extend(std::iter::repeat_n(byte, record.wRepeatCount.into()));
                }
            }
            WINDOW_BUFFER_SIZE_EVENT => {
                if !bytes.is_empty() {
                    output.push(TerminalSourceInput::Bytes(mem::take(&mut bytes)));
                }
                let size = unsafe { record.Event.WindowBufferSizeEvent }.dwSize;
                if size.X > 0 && size.Y > 0 {
                    output.push(TerminalSourceInput::Event(crossterm::event::Event::Resize(
                        size.X as u16,
                        size.Y as u16,
                    )));
                }
            }
            _ => {}
        }
    }
    if !bytes.is_empty() {
        output.push(TerminalSourceInput::Bytes(bytes));
    }
    output
}

#[derive(Default)]
struct SeenTerminalReplies {
    palette: [bool; 16],
    foreground: bool,
    background: bool,
}

fn apply_osc_color(
    payload: &str,
    probe: &mut TerminalColorProbe,
    seen: &mut SeenTerminalReplies,
) -> bool {
    let mut fields = payload.split(';');
    match fields.next() {
        Some("4") => {
            let mut found = false;
            while let (Some(index), Some(color)) = (fields.next(), fields.next()) {
                let Some(index) = index.parse::<usize>().ok().filter(|index| *index < 16) else {
                    continue;
                };
                let Some(color) = parse_osc_rgb(color) else {
                    continue;
                };
                probe.palette[index] = Some(color);
                seen.palette[index] = true;
                found = true;
            }
            found
        }
        Some("10") => fields.next().and_then(parse_osc_rgb).is_some_and(|color| {
            probe.foreground = Some(color);
            seen.foreground = true;
            true
        }),
        Some("11") => fields.next().and_then(parse_osc_rgb).is_some_and(|color| {
            probe.background = Some(color);
            seen.background = true;
            true
        }),
        _ => false,
    }
}

fn parse_osc_rgb(value: &str) -> Option<TerminalColor> {
    let (format, components) = value.trim().split_once(':')?;
    if !format.eq_ignore_ascii_case("rgb") {
        return None;
    }
    let mut components = components.split('/');
    let red = parse_osc_component(components.next()?)?;
    let green = parse_osc_component(components.next()?)?;
    let blue = parse_osc_component(components.next()?)?;
    components
        .next()
        .is_none()
        .then_some(TerminalColor::new(red, green, blue))
}

fn parse_osc_component(component: &str) -> Option<u8> {
    if !(1..=4).contains(&component.len()) {
        return None;
    }
    let value = u32::from(u16::from_str_radix(component, 16).ok()?);
    let maximum = (1_u32 << (component.len() * 4)) - 1;
    Some((value * u32::from(u8::MAX) / maximum) as u8)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CellSize, GraphicsProtocol, GraphicsReply, NoGraphics, TerminalColor, TerminalColorProbe,
        TerminalEvents, TerminalFacts, TerminalInput, TerminalInputParser, TerminalSourceInput,
        request_terminal_colors, request_terminal_probe,
    };
    use crossterm::event::{
        Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

    /// The Kitty graphics reply accepting the probe's query.
    const KITTY_OK: &[u8] = b"\x1b_Gi=31;OK\x1b\\";
    /// XTVERSION naming a terminal that draws iTerm2 inline images.
    const WEZTERM_VERSION: &[u8] = b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\";
    /// Primary device attributes carrying attribute 4, Sixel.
    const SIXEL_ATTRIBUTES: &[u8] = b"\x1b[?62;4;22c";
    /// Primary device attributes without Sixel, which every probe ends with.
    const PLAIN_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";
    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
    };

    /// A terminal whose window reported its pixels, so its cell size is known.
    fn measured() -> TerminalFacts {
        TerminalFacts::unprobed(true).with_cell_size(Some(CELL))
    }

    /// What `facts` become once the parser has read the terminal's `replies`,
    /// every one of which must be a graphics reply rather than reader input.
    fn facts_after(mut facts: TerminalFacts, replies: &[u8]) -> TerminalFacts {
        for input in TerminalInputParser::default().parse(replies, false) {
            match input {
                TerminalInput::Graphics(reply) => facts.merge_graphics(&reply),
                other => panic!("a probe reply leaked into reader input as {other:?}"),
            }
        }
        facts
    }

    fn joined(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> TerminalInput {
        TerminalInput::Event(InputEvent::Key(KeyEvent::new(code, modifiers)))
    }

    fn record_log(action: impl FnOnce()) -> String {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("terminal.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::sync::Arc::new(std::fs::File::create(&path).unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_kitty_reply_selects_kitty() {
        let facts = facts_after(measured(), &joined(&[KITTY_OK, PLAIN_ATTRIBUTES]));

        assert_eq!(facts.graphics, Some(GraphicsProtocol::Kitty));
        assert_eq!(facts.cell_size, Some(CELL));
    }

    #[test]
    fn a_kitty_reply_refusing_the_query_selects_nothing_and_never_becomes_keys() {
        let facts = facts_after(
            measured(),
            &joined(&[
                b"\x1b_Gi=31;EINVAL:unsupported format\x1b\\",
                PLAIN_ATTRIBUTES,
            ]),
        );

        assert_eq!(facts.graphics, None);
    }

    #[test]
    fn device_attributes_select_sixel_only_when_an_attribute_is_four() {
        for (reply, expected) in [
            (SIXEL_ATTRIBUTES, Some(GraphicsProtocol::Sixel)),
            (
                b"\x1b[?64;1;2;4;6;9;15;18;21;22c".as_slice(),
                Some(GraphicsProtocol::Sixel),
            ),
            (PLAIN_ATTRIBUTES, None),
            (b"\x1b[?1;2c".as_slice(), None),
            // The first parameter is the terminal's class rather than an
            // attribute, and class 4 is a VT132, which draws no Sixel.
            (b"\x1b[?4;6c".as_slice(), None),
        ] {
            assert_eq!(
                facts_after(measured(), reply).graphics,
                expected,
                "{}",
                String::from_utf8_lossy(reply).escape_debug()
            );
        }
    }

    #[test]
    fn xtversion_selects_iterm2_for_each_terminal_known_to_draw_its_inline_images() {
        for name in [
            "iTerm2 3.5.0",
            "WezTerm 20240203-110809-5046fc22",
            "VSCode 1.90",
            "rio 0.1.0",
            "mintty 3.7.0",
        ] {
            let version = format!("\x1bP>|{name}\x1b\\");

            let facts = facts_after(measured(), &joined(&[version.as_bytes(), PLAIN_ATTRIBUTES]));

            assert_eq!(facts.graphics, Some(GraphicsProtocol::Iterm2), "{name}");
        }
    }

    #[test]
    fn xtversion_naming_any_other_terminal_selects_nothing() {
        for name in [
            "XTerm(390)",
            "foot(1.16.2)",
            "riot 1.0",
            "Apple_Terminal",
            "",
        ] {
            let version = format!("\x1bP>|{name}\x1b\\");

            let facts = facts_after(measured(), &joined(&[version.as_bytes(), PLAIN_ATTRIBUTES]));

            assert_eq!(facts.graphics, None, "{name:?}");
        }
    }

    #[test]
    fn a_pixel_size_reply_gives_the_cell_size_the_window_did_not() {
        let facts = facts_after(
            TerminalFacts::unprobed(true),
            &joined(&[KITTY_OK, b"\x1b[6;20;10t", PLAIN_ATTRIBUTES]),
        );

        assert_eq!(
            facts.cell_size,
            Some(CELL),
            "the reply is height then width"
        );
        assert_eq!(facts.graphics, Some(GraphicsProtocol::Kitty));
    }

    #[test]
    fn a_window_reporting_its_pixels_gives_the_cell_size_and_one_reporting_none_gives_nothing() {
        use ratatui::{backend::WindowSize, layout::Size};

        let window = |pixels: Size| WindowSize {
            columns_rows: Size::new(100, 40),
            pixels,
        };

        assert_eq!(
            CellSize::from_window(window(Size::new(1000, 800))),
            Some(CELL)
        );
        assert_eq!(CellSize::from_window(window(Size::new(0, 0))), None);
        assert_eq!(
            CellSize::from_window(WindowSize {
                columns_rows: Size::new(0, 0),
                pixels: Size::new(1000, 800),
            }),
            None
        );
    }

    #[test]
    fn several_answers_select_kitty_then_iterm2_then_sixel() {
        assert_eq!(
            facts_after(
                measured(),
                &joined(&[KITTY_OK, WEZTERM_VERSION, SIXEL_ATTRIBUTES])
            )
            .graphics,
            Some(GraphicsProtocol::Kitty)
        );
        assert_eq!(
            facts_after(measured(), &joined(&[WEZTERM_VERSION, SIXEL_ATTRIBUTES])).graphics,
            Some(GraphicsProtocol::Iterm2)
        );
        assert_eq!(
            facts_after(measured(), SIXEL_ATTRIBUTES).graphics,
            Some(GraphicsProtocol::Sixel)
        );
    }

    #[test]
    fn tmux_and_screen_select_nothing_whatever_the_terminal_answers() {
        let every_answer = joined(&[KITTY_OK, WEZTERM_VERSION, SIXEL_ATTRIBUTES]);

        let multiplexed = facts_after(measured().with_multiplexer(true), &every_answer);
        assert_eq!(multiplexed.graphics, None);
        assert_eq!(
            multiplexed.graphics_selection(),
            Err(NoGraphics::Multiplexed)
        );

        // A multiplexer reached where its environment is not inherited, over
        // ssh say, still names itself when asked.
        for name in ["tmux 3.4", "screen 4.09"] {
            let version = format!("\x1bP>|{name}\x1b\\");
            let facts = facts_after(
                measured(),
                &joined(&[KITTY_OK, version.as_bytes(), SIXEL_ATTRIBUTES]),
            );
            assert_eq!(facts.graphics, None, "{name}");
        }
    }

    #[test]
    fn an_unknown_cell_size_selects_nothing_until_the_terminal_reports_one() {
        let facts = facts_after(
            TerminalFacts::unprobed(true),
            &joined(&[KITTY_OK, PLAIN_ATTRIBUTES]),
        );

        assert_eq!(facts.graphics, None);
        assert_eq!(facts.graphics_selection(), Err(NoGraphics::UnknownCellSize));
        assert_eq!(
            facts_after(facts, b"\x1b[6;20;10t").graphics,
            Some(GraphicsProtocol::Kitty),
            "a late cell size completes the selection"
        );
    }

    #[test]
    fn a_terminal_that_ignores_the_probe_keeps_its_input_and_selects_nothing() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();

        let observed = parser.parse(b"hi\x1b[A", false);

        assert_eq!(
            observed,
            vec![
                key(KeyCode::Char('h'), KeyModifiers::NONE),
                key(KeyCode::Char('i'), KeyModifiers::NONE),
                key(KeyCode::Up, KeyModifiers::NONE),
            ]
        );
        let facts = measured();
        assert_eq!(facts.graphics, None);
        assert_eq!(facts.graphics_selection(), Err(NoGraphics::NoProtocol));
    }

    #[test]
    fn split_graphics_replies_are_routed_away_from_key_events() {
        let mut parser = TerminalInputParser::default();
        let replies = joined(&[
            KITTY_OK,
            WEZTERM_VERSION,
            b"\x1b[6;20;10t",
            SIXEL_ATTRIBUTES,
        ]);
        let mut observed = Vec::new();

        for byte in &replies {
            observed.extend(parser.parse(&[*byte], true));
        }
        observed.extend(parser.parse(&[], false));

        assert_eq!(
            observed,
            vec![
                TerminalInput::Graphics(GraphicsReply::Kitty),
                TerminalInput::Graphics(GraphicsReply::Version(
                    "WezTerm 20240203-110809-5046fc22".to_owned()
                )),
                TerminalInput::Graphics(GraphicsReply::CellSize(CELL)),
                TerminalInput::Graphics(GraphicsReply::DeviceAttributes { sixel: true }),
            ]
        );
    }

    #[test]
    fn alt_chords_that_begin_no_graphics_reply_remain_reader_input() {
        let mut parser = TerminalInputParser::default();

        assert_eq!(
            parser.parse(b"\x1b_x", false),
            vec![
                key(KeyCode::Char('_'), KeyModifiers::ALT),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
            ]
        );
        assert_eq!(
            parser.parse(b"\x1bPx", false),
            vec![
                key(KeyCode::Char('P'), KeyModifiers::ALT | KeyModifiers::SHIFT),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
            ]
        );
        assert_eq!(
            parser.parse(b"\x1b_", false),
            vec![key(KeyCode::Char('_'), KeyModifiers::ALT)],
            "a standalone chord resolves at the input boundary"
        );
    }

    #[test]
    fn keys_sharing_a_prefix_with_the_graphics_replies_remain_keys() {
        let mut parser = TerminalInputParser::default();
        let mut observed = Vec::new();

        for byte in b"\x1b[6~\x1b[6;5~\x1b[?997;1n" {
            observed.extend(parser.parse(&[*byte], true));
        }
        observed.extend(parser.parse(&[], false));

        assert_eq!(
            observed,
            vec![
                key(KeyCode::PageDown, KeyModifiers::NONE),
                key(KeyCode::PageDown, KeyModifiers::CONTROL),
                TerminalInput::Reprobe,
            ]
        );
    }

    #[test]
    fn graphics_shaped_bytes_inside_bracketed_paste_remain_pasted_text() {
        let mut parser = TerminalInputParser::default();
        let text = "a\x1b_Gi=31;OK\x1b\\b\x1b[?62;4;22cc\x1b[6;20;10td";
        let bytes = format!("\x1b[200~{text}\x1b[201~");
        let mut observed = Vec::new();

        for byte in bytes.bytes() {
            observed.extend(parser.parse(&[byte], true));
        }
        observed.extend(parser.parse(&[], false));

        assert_eq!(
            observed,
            vec![TerminalInput::Event(InputEvent::Paste(text.to_owned()))]
        );
    }

    /// The Log lines a run of `replies` merged into `facts` writes.
    fn logged_after(mut facts: TerminalFacts, replies: &[GraphicsReply]) -> Vec<String> {
        let log = record_log(|| {
            for reply in replies {
                facts.merge_graphics(reply);
            }
        });
        log.lines().map(str::to_owned).collect()
    }

    #[test]
    fn the_log_records_the_selection_once_when_the_device_attributes_settle_the_probe() {
        let lines = logged_after(
            measured(),
            &[
                GraphicsReply::Kitty,
                GraphicsReply::DeviceAttributes { sixel: false },
                GraphicsReply::DeviceAttributes { sixel: false },
            ],
        );

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("INFO"), "{lines:?}");
        assert!(lines[0].contains("Kitty"), "{lines:?}");
        assert!(lines[0].contains("10x20"), "{lines:?}");
    }

    #[test]
    fn the_log_says_why_no_protocol_was_selected_when_the_probe_settles() {
        for (facts, replies, reason) in [
            (
                measured().with_multiplexer(true),
                vec![GraphicsReply::DeviceAttributes { sixel: true }],
                "tmux or screen",
            ),
            (
                measured(),
                vec![GraphicsReply::DeviceAttributes { sixel: false }],
                "no graphics protocol",
            ),
            (
                TerminalFacts::unprobed(true),
                vec![GraphicsReply::Kitty, GraphicsReply::Expired],
                "cell pixel size",
            ),
        ] {
            let lines = logged_after(facts, &replies);

            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("INFO"), "{lines:?}");
            assert!(lines[0].contains(reason), "{lines:?}");
        }
    }

    #[test]
    fn nothing_is_logged_before_the_probe_settles() {
        assert_eq!(
            logged_after(
                measured(),
                &[
                    GraphicsReply::Kitty,
                    GraphicsReply::CellSize(CELL),
                    GraphicsReply::Version("WezTerm 20240203".to_owned()),
                ],
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_reply_after_the_probe_expired_logs_the_selection_it_changes_to() {
        let lines = logged_after(
            measured(),
            &[
                GraphicsReply::Expired,
                GraphicsReply::Kitty,
                GraphicsReply::DeviceAttributes { sixel: true },
            ],
        );

        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("no graphics protocol"), "{lines:?}");
        assert!(
            lines[1].contains("changed after the probe settled") && lines[1].contains("Kitty"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_partial_cell_size_reply_survives_an_idle_boundary_while_the_probe_is_outstanding() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();

        assert_eq!(parser.parse(b"\x1b[6;20;", false), vec![]);
        assert_eq!(
            parser.parse(b"10t", false),
            vec![TerminalInput::Graphics(GraphicsReply::CellSize(CELL))]
        );
    }

    #[test]
    fn partial_device_attributes_survive_an_idle_boundary_while_the_probe_is_outstanding() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();

        assert_eq!(parser.parse(b"\x1b[?62;", false), vec![]);
        assert_eq!(
            parser.parse(b"4c", false),
            vec![TerminalInput::Graphics(GraphicsReply::DeviceAttributes {
                sixel: true
            })]
        );
    }

    #[test]
    fn a_csi_split_before_its_reply_is_identified_survives_an_idle_boundary() {
        for (first, rest, expected) in [
            (
                b"\x1b[6".as_slice(),
                b";20;10t".as_slice(),
                GraphicsReply::CellSize(CELL),
            ),
            (
                b"\x1b[".as_slice(),
                b"?62;4c".as_slice(),
                GraphicsReply::DeviceAttributes { sixel: true },
            ),
            (
                b"\x1b[".as_slice(),
                b"6;20;10t".as_slice(),
                GraphicsReply::CellSize(CELL),
            ),
        ] {
            let mut parser = TerminalInputParser::default();
            parser.expect_replies();

            assert_eq!(parser.parse(first, false), vec![]);
            assert_eq!(
                parser.parse(rest, false),
                vec![TerminalInput::Graphics(expected)],
                "{}",
                String::from_utf8_lossy(first).escape_debug()
            );
        }
    }

    #[test]
    fn a_key_completing_a_held_csi_prefix_during_the_reply_window_is_still_that_key() {
        for (first, rest, expected) in [
            (
                b"\x1b[".as_slice(),
                b"A".as_slice(),
                key(KeyCode::Up, KeyModifiers::NONE),
            ),
            (
                b"\x1b[6".as_slice(),
                b"~".as_slice(),
                key(KeyCode::PageDown, KeyModifiers::NONE),
            ),
            (
                b"\x1b[6;".as_slice(),
                b"5~".as_slice(),
                key(KeyCode::PageDown, KeyModifiers::CONTROL),
            ),
        ] {
            let mut parser = TerminalInputParser::default();
            parser.expect_replies();

            assert_eq!(parser.parse(first, false), vec![]);
            assert_eq!(
                parser.parse(rest, false),
                vec![expected],
                "{}",
                String::from_utf8_lossy(first).escape_debug()
            );
        }
    }

    #[test]
    fn a_paste_end_marker_split_at_an_idle_gap_still_ends_the_paste() {
        for (first, rest) in [
            (b"\x1b[200~hello\x1b[".as_slice(), b"201~".as_slice()),
            (b"\x1b[200~hello\x1b".as_slice(), b"[201~".as_slice()),
            (b"\x1b[200~hello\x1b[20".as_slice(), b"1~".as_slice()),
        ] {
            for expecting in [false, true] {
                let mut parser = TerminalInputParser::default();
                if expecting {
                    parser.expect_replies();
                }

                assert_eq!(parser.parse(first, false), vec![]);
                assert_eq!(
                    parser.parse(rest, false),
                    vec![TerminalInput::Event(InputEvent::Paste("hello".to_owned()))],
                    "{} while expecting replies: {expecting}",
                    String::from_utf8_lossy(first).escape_debug()
                );
                assert_eq!(
                    parser.parse(b"x", false),
                    vec![key(KeyCode::Char('x'), KeyModifiers::NONE)],
                    "typing after the paste is typing again"
                );
            }
        }
    }

    #[test]
    fn an_escape_bracket_pasted_across_an_idle_gap_stays_in_the_pasted_text() {
        let mut parser = TerminalInputParser::default();

        assert_eq!(parser.parse(b"\x1b[200~a\x1b[", false), vec![]);
        assert_eq!(
            parser.parse(b"b\x1b[201~", false),
            vec![TerminalInput::Event(InputEvent::Paste(
                "a\x1b[b".to_owned()
            ))]
        );
    }

    #[test]
    fn an_escape_pasted_before_the_end_marker_still_lets_the_marker_end_the_paste() {
        let mut parser = TerminalInputParser::default();

        assert_eq!(parser.parse(b"\x1b[200~hello\x1b", false), vec![]);
        assert_eq!(
            parser.parse(b"\x1b[201~", false),
            vec![TerminalInput::Event(InputEvent::Paste(
                "hello\x1b".to_owned()
            ))]
        );
        assert_eq!(
            parser.parse(b"\x1b", false),
            vec![key(KeyCode::Esc, KeyModifiers::NONE)],
            "an Escape after the paste is a key again"
        );
    }

    #[test]
    fn escapes_pasted_across_an_idle_gap_stay_in_the_pasted_text() {
        let mut parser = TerminalInputParser::default();

        assert_eq!(parser.parse(b"\x1b[200~a\x1b", false), vec![]);
        assert_eq!(
            parser.parse(b"\x1bb\x1b[201~", false),
            vec![TerminalInput::Event(InputEvent::Paste(
                "a\x1b\x1bb".to_owned()
            ))]
        );
    }

    #[test]
    fn an_escape_cutting_a_held_csi_short_begins_the_next_sequence() {
        let mut parser = TerminalInputParser::default();
        assert_eq!(parser.parse(b"\x1b[200~a\x1b[20", false), vec![]);
        assert_eq!(
            parser.parse(b"\x1b[201~", false),
            vec![TerminalInput::Event(InputEvent::Paste(
                "a\x1b[20".to_owned()
            ))],
            "inside a paste, the end marker after a cut-short prefix still ends it"
        );

        let mut parser = TerminalInputParser::default();
        parser.expect_replies();
        assert_eq!(parser.parse(b"\x1b[", false), vec![]);
        assert_eq!(
            parser.parse(b"\x1b[A", false),
            vec![
                key(KeyCode::Char('['), KeyModifiers::ALT),
                key(KeyCode::Up, KeyModifiers::NONE),
            ],
            "outside one, a held introducer is the chord and the escape begins the next key"
        );
    }

    #[test]
    fn a_held_csi_introducer_is_alt_bracket_once_the_reply_window_closes() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();
        assert_eq!(parser.parse(b"\x1b[", false), vec![]);

        parser.stop_expecting_replies();

        assert_eq!(
            parser.parse(&[], false),
            vec![key(KeyCode::Char('['), KeyModifiers::ALT)]
        );
    }

    #[test]
    fn a_partial_csi_reply_is_released_once_the_reply_window_closes() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();
        assert_eq!(parser.parse(b"\x1b[?62;", false), vec![]);

        parser.stop_expecting_replies();
        parser.parse(&[], false);

        assert!(
            matches!(parser.state, super::RawInputState::Ground),
            "the prefix is the reader's input again"
        );
        assert!(
            !parser
                .parse(b"4c", false)
                .iter()
                .any(|input| matches!(input, TerminalInput::Graphics(_))),
            "so its completion is no longer taken for a reply"
        );
    }

    #[test]
    fn a_control_string_ended_before_its_reply_prefix_remains_reader_input() {
        let ctrl_g = || key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let alt_underscore = || key(KeyCode::Char('_'), KeyModifiers::ALT);
        let alt_shift_p = || key(KeyCode::Char('P'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        for (bytes, expected) in [
            (b"\x1b_\x07".as_slice(), vec![alt_underscore(), ctrl_g()]),
            (
                b"\x1b_x".as_slice(),
                vec![
                    alt_underscore(),
                    key(KeyCode::Char('x'), KeyModifiers::NONE),
                ],
            ),
            (b"\x1bP\x07".as_slice(), vec![alt_shift_p(), ctrl_g()]),
            (
                b"\x1bPx".as_slice(),
                vec![alt_shift_p(), key(KeyCode::Char('x'), KeyModifiers::NONE)],
            ),
            (
                b"\x1bP>\x07".as_slice(),
                vec![
                    alt_shift_p(),
                    key(KeyCode::Char('>'), KeyModifiers::NONE),
                    ctrl_g(),
                ],
            ),
            (
                b"\x1b]\x07".as_slice(),
                vec![key(KeyCode::Char(']'), KeyModifiers::ALT), ctrl_g()],
            ),
            // An escape before the prefix begins the reader's next key, which
            // is read as a key rather than folded into the string.
            (
                b"\x1b_\x1b[A".as_slice(),
                vec![alt_underscore(), key(KeyCode::Up, KeyModifiers::NONE)],
            ),
        ] {
            let mut parser = TerminalInputParser::default();
            parser.expect_replies();

            assert_eq!(
                parser.parse(bytes, false),
                expected,
                "{}",
                String::from_utf8_lossy(bytes).escape_debug()
            );
        }
    }

    #[test]
    fn a_control_string_whose_reply_prefix_matched_is_swallowed_to_its_terminator() {
        for bytes in [
            b"\x1b_Gx\x07".as_slice(),
            b"\x1b_Ga=q,i=31;OK\x1b\\",
            b"\x1bP>|\x1b\\",
        ] {
            assert!(
                !TerminalInputParser::default()
                    .parse(bytes, false)
                    .iter()
                    .any(|input| matches!(input, TerminalInput::Event(_))),
                "{}",
                String::from_utf8_lossy(bytes).escape_debug()
            );
        }
        assert_eq!(
            TerminalInputParser::default().parse(b"\x1b_Ga=q,i=31;OK\x1b\\", false),
            vec![TerminalInput::Graphics(GraphicsReply::Kitty)],
            "a Kitty reply is recognized whatever order its keys come in"
        );
    }

    #[test]
    fn a_terminal_program_known_to_draw_iterm2_images_selects_iterm2_without_xtversion() {
        for program in ["vscode", "iTerm.app", "WezTerm", "rio", "mintty", "VSCode"] {
            let facts = facts_after(
                measured().with_terminal_program(Some(program)),
                PLAIN_ATTRIBUTES,
            );

            assert_eq!(facts.graphics, Some(GraphicsProtocol::Iterm2), "{program}");
        }

        let unsettled = measured().with_terminal_program(Some("vscode"));
        assert_eq!(
            unsettled.graphics, None,
            "the name is consulted only once the probe says no XTVERSION reply is coming"
        );
        let mut expired = unsettled;
        expired.merge_graphics(&GraphicsReply::Expired);
        assert_eq!(expired.graphics, Some(GraphicsProtocol::Iterm2));
        assert_eq!(
            facts_after(
                TerminalFacts::unprobed(true).with_terminal_program(Some("vscode")),
                PLAIN_ATTRIBUTES,
            )
            .graphics,
            None,
            "not without a cell size"
        );
    }

    /// A terminal launched from inside another inherits its TERM_PROGRAM, so
    /// the name a terminal gives XTVERSION for itself decides.
    #[test]
    fn an_xtversion_name_decides_over_an_inherited_terminal_program() {
        let foot = facts_after(
            measured().with_terminal_program(Some("vscode")),
            b"\x1bP>|foot 1.16\x1b\\\x1b[?62;22c",
        );
        assert_eq!(foot.graphics, None);
        assert_eq!(foot.graphics_selection(), Err(NoGraphics::NoProtocol));

        let wezterm = facts_after(
            measured().with_terminal_program(Some("Apple_Terminal")),
            &joined(&[
                b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\",
                PLAIN_ATTRIBUTES,
            ]),
        );
        assert_eq!(wezterm.graphics, Some(GraphicsProtocol::Iterm2));
    }

    #[test]
    fn an_unlisted_terminal_program_selects_nothing() {
        for program in [Some("Apple_Terminal"), Some("ghostty"), Some(""), None] {
            let facts = facts_after(measured().with_terminal_program(program), PLAIN_ATTRIBUTES);

            assert_eq!(facts.graphics, None, "{program:?}");
        }
    }

    #[test]
    fn a_multiplexer_suppresses_a_terminal_program_known_to_draw_iterm2_images() {
        let facts = facts_after(
            measured()
                .with_terminal_program(Some("vscode"))
                .with_multiplexer(true),
            PLAIN_ATTRIBUTES,
        );

        assert_eq!(facts.graphics, None);
        assert_eq!(facts.graphics_selection(), Err(NoGraphics::Multiplexed));
    }

    #[test]
    fn facts_built_for_a_protocol_select_it_by_the_usual_preference() {
        for protocol in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Iterm2,
            GraphicsProtocol::Sixel,
        ] {
            assert_eq!(
                measured().with_graphics_answer(protocol).graphics,
                Some(protocol)
            );
        }
        assert_eq!(
            measured()
                .with_graphics_answer(GraphicsProtocol::Sixel)
                .with_graphics_answer(GraphicsProtocol::Kitty)
                .graphics,
            Some(GraphicsProtocol::Kitty)
        );
    }

    #[test]
    fn probe_writes_the_color_queries_then_the_graphics_queries_ending_with_device_attributes() {
        let mut output = Vec::new();

        request_terminal_probe(&mut output, &TerminalFacts::unprobed(true)).unwrap();

        let mut expected = Vec::new();
        request_terminal_colors(&mut expected).unwrap();
        expected
            .extend_from_slice(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[>0q\x1b[16t\x1b[c");
        assert_eq!(output, expected);
    }

    #[test]
    fn a_probe_whose_window_reported_its_pixels_does_not_ask_for_the_cell_size() {
        let mut output = Vec::new();

        request_terminal_probe(&mut output, &measured()).unwrap();

        let mut expected = Vec::new();
        request_terminal_colors(&mut expected).unwrap();
        expected.extend_from_slice(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[>0q\x1b[c");
        assert_eq!(output, expected);
    }

    #[test]
    fn a_probe_under_a_multiplexer_asks_for_device_attributes_alone() {
        let mut output = Vec::new();

        request_terminal_probe(&mut output, &measured().with_multiplexer(true)).unwrap();

        let mut expected = Vec::new();
        request_terminal_colors(&mut expected).unwrap();
        expected.extend_from_slice(b"\x1b[c");
        assert_eq!(output, expected);
    }

    #[cfg(windows)]
    fn windows_key(
        byte: u8,
        repeat: u16,
        down: bool,
    ) -> windows_sys::Win32::System::Console::INPUT_RECORD {
        use windows_sys::Win32::System::Console::{
            INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
        };

        INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: i32::from(down),
                    wRepeatCount: repeat,
                    wVirtualKeyCode: 0,
                    wVirtualScanCode: 0,
                    uChar: KEY_EVENT_RECORD_0 {
                        AsciiChar: byte as i8,
                    },
                    dwControlKeyState: 0,
                },
            },
        }
    }

    #[cfg(windows)]
    fn windows_resize(
        columns: i16,
        rows: i16,
    ) -> windows_sys::Win32::System::Console::INPUT_RECORD {
        use windows_sys::Win32::System::Console::{
            COORD, INPUT_RECORD, INPUT_RECORD_0, WINDOW_BUFFER_SIZE_EVENT,
            WINDOW_BUFFER_SIZE_RECORD,
        };

        INPUT_RECORD {
            EventType: WINDOW_BUFFER_SIZE_EVENT as u16,
            Event: INPUT_RECORD_0 {
                WindowBufferSizeEvent: WINDOW_BUFFER_SIZE_RECORD {
                    dwSize: COORD {
                        X: columns,
                        Y: rows,
                    },
                },
            },
        }
    }

    #[test]
    fn owned_input_parser_preserves_ordinary_terminal_events() {
        let mut parser = TerminalInputParser::default();

        let observed = parser.parse(b"x\x1b[A\x1b[200~pasted\ntext\x1b[201~\x1b[<0;2;3M", false);

        assert_eq!(
            observed,
            vec![
                TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                ))),
                TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Up,
                    KeyModifiers::NONE,
                ))),
                TerminalInput::Event(InputEvent::Paste("pasted\ntext".to_owned())),
                TerminalInput::Event(InputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                })),
            ]
        );
    }

    #[test]
    fn alt_bracket_that_is_not_a_color_reply_remains_reader_input() {
        let mut parser = TerminalInputParser::default();

        let observed = parser.parse(b"\x1b]x", false);

        assert_eq!(
            observed,
            vec![
                TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char(']'),
                    KeyModifiers::ALT,
                ))),
                TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                ))),
            ]
        );
    }

    #[test]
    fn standalone_alt_bracket_resolves_at_the_input_boundary() {
        let mut parser = TerminalInputParser::default();

        let observed = parser.parse(b"\x1b]", false);

        assert_eq!(
            observed,
            vec![TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT,
            )))]
        );
    }

    #[test]
    fn oversized_color_response_is_discarded_instead_of_becoming_keys() {
        let mut parser = TerminalInputParser::default();
        parser.expect_replies();
        let mut response = b"\x1b]11;".to_vec();
        response.extend(std::iter::repeat_n(b'x', 70 * 1024));
        response.extend_from_slice(b"\x07z");

        let observed = parser.parse(&response, false);

        assert_eq!(
            observed,
            vec![TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            )))]
        );
    }

    #[test]
    fn osc_shaped_bytes_inside_bracketed_paste_remain_pasted_text() {
        let mut parser = TerminalInputParser::default();
        let text = "before\x1b]11;rgb:ffff/ffff/ffff\x07after";
        let bytes = format!("\x1b[200~{text}\x1b[201~");
        let mut observed = Vec::new();

        for byte in bytes.bytes() {
            observed.extend(parser.parse(&[byte], true));
        }
        observed.extend(parser.parse(&[], false));

        assert_eq!(
            observed,
            vec![TerminalInput::Event(InputEvent::Paste(text.to_owned()))]
        );
    }

    #[test]
    fn owned_input_parser_routes_split_color_replies_away_from_key_events() {
        let mut parser = TerminalInputParser::default();
        let response = b"\x1b]4;0;rgb:0000/1111/2222;15;rgb:ffff/eeee/dddd\x1b\\\x1b]10;rgb:aaaa/bbbb/cccc\x07\x1b]11;rgb:1234/5678/9abc\x1b\\";
        let mut observed = Vec::new();

        for byte in response {
            observed.extend(parser.parse(&[*byte], true));
        }
        observed.extend(parser.parse(&[], false));

        let mut palette = [None; 16];
        palette[0] = Some(TerminalColor::new(0, 17, 34));
        palette[15] = Some(TerminalColor::new(255, 238, 221));
        assert_eq!(
            observed,
            vec![
                TerminalInput::Colors(TerminalColorProbe::new(palette, None, None)),
                TerminalInput::Colors(TerminalColorProbe::new(
                    [None; 16],
                    Some(TerminalColor::new(170, 187, 204)),
                    None,
                )),
                TerminalInput::Colors(TerminalColorProbe::new(
                    [None; 16],
                    None,
                    Some(TerminalColor::new(18, 86, 154)),
                )),
            ]
        );
    }

    #[test]
    fn late_color_reply_survives_idle_boundaries_without_becoming_keys() {
        let mut parser = TerminalInputParser::default();

        assert!(parser.parse(b"\x1b]11;", false).is_empty());
        assert!(parser.parse(b"rgb:ffff/ffff/ffff\x1b", false).is_empty());
        assert_eq!(
            parser.parse(b"\\", false),
            vec![TerminalInput::Colors(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(255, 255, 255)),
            ))]
        );
    }

    #[test]
    fn owned_input_parser_routes_theme_reports_to_a_reprobe() {
        let mut parser = TerminalInputParser::default();
        let mut observed = Vec::new();

        for byte in b"\x1b[?997;1n\x1b[?997;2n" {
            observed.extend(parser.parse(&[*byte], true));
        }
        observed.extend(parser.parse(&[], false));

        assert_eq!(
            observed,
            vec![TerminalInput::Reprobe, TerminalInput::Reprobe]
        );
    }

    #[test]
    fn hyperlink_detection_is_allowlisted_and_conservative_inside_multiplexers() {
        for program in ["WezTerm", "iTerm.app", "vscode", "ghostty", "Hyper"] {
            assert!(super::TerminalFacts::supports_hyperlinks(
                Some(program),
                false
            ));
            assert!(!super::TerminalFacts::supports_hyperlinks(
                Some(program),
                true
            ));
        }
        assert!(!super::TerminalFacts::supports_hyperlinks(
            Some("Apple_Terminal"),
            false
        ));
        assert!(!super::TerminalFacts::supports_hyperlinks(None, false));
    }

    #[test]
    fn late_color_replies_merge_with_the_terminal_facts_already_observed() {
        let mut palette = [None; 16];
        palette[1] = Some(TerminalColor::new(10, 20, 30));
        let mut facts = super::TerminalFacts::new(
            Some(TerminalColorProbe::new(
                palette,
                Some(TerminalColor::new(220, 220, 220)),
                Some(TerminalColor::new(20, 20, 20)),
            )),
            true,
        )
        .with_hyperlinks(true);
        let mut update = [None; 16];
        update[1] = Some(TerminalColor::new(30, 20, 10));
        update[2] = Some(TerminalColor::new(40, 50, 60));

        facts.merge_probe(TerminalColorProbe::new(
            update,
            None,
            Some(TerminalColor::new(250, 250, 250)),
        ));

        assert_eq!(facts.probe.unwrap().palette[0], None);
        assert_eq!(
            facts.probe.unwrap().palette[1],
            Some(TerminalColor::new(30, 20, 10))
        );
        assert_eq!(
            facts.probe.unwrap().palette[2],
            Some(TerminalColor::new(40, 50, 60))
        );
        assert_eq!(
            facts.probe.unwrap().foreground,
            Some(TerminalColor::new(220, 220, 220))
        );
        assert_eq!(
            facts.probe.unwrap().background,
            Some(TerminalColor::new(250, 250, 250))
        );
        assert!(
            facts.hyperlinks,
            "a color reply cannot erase OSC 8 capability"
        );
    }

    #[test]
    fn owned_input_parser_preserves_navigation_function_and_control_keys() {
        let mut parser = TerminalInputParser::default();

        let observed = parser.parse(b"\x1b[1;5D\x1b[15~\x7f\r\t", false);

        assert_eq!(
            observed,
            [
                (KeyCode::Left, KeyModifiers::CONTROL),
                (KeyCode::F(5), KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Tab, KeyModifiers::NONE),
            ]
            .into_iter()
            .map(
                |(code, modifiers)| TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    code, modifiers
                )))
            )
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn color_probe_writes_the_sixteen_palette_and_default_color_queries() {
        let mut output = Vec::new();

        request_terminal_colors(&mut output).unwrap();

        let mut expected = Vec::new();
        for index in 0..16 {
            expected.extend_from_slice(format!("\x1b]4;{index};?\x1b\\").as_bytes());
        }
        expected.extend_from_slice(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
        assert_eq!(output, expected);
    }

    #[tokio::test]
    async fn startup_color_queries_do_not_wait_for_reader_input() {
        let (_sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();
        let mut output = Vec::new();
        // Returning synchronously with the sender still open proves that no
        // input, EOF, or probe timeout is needed to proceed to the first frame.
        events
            .request_probe(&mut output, &TerminalFacts::default())
            .unwrap();
        let mut colors = Vec::new();
        request_terminal_colors(&mut colors).unwrap();
        assert!(output.starts_with(&colors));
        assert!(output.ends_with(super::PRIMARY_DEVICE_ATTRIBUTES_QUERY));

        let workspace = tempfile::tempdir().unwrap();
        let mut application =
            crate::tui::Application::new(workspace.path(), super::TerminalFacts::unprobed(true));
        application
            .handle_event(crate::tui::ApplicationEvent::Managed(
                crate::managed_client::ManagedEvent::SettingsSnapshot(
                    crate::protocol::SettingsSnapshot::default(),
                ),
            ))
            .unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| application.render(frame)).unwrap();
        let shown: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(shown.contains("Type a prompt, run a /command, use a $skill"));
    }

    #[tokio::test]
    async fn startup_queries_preserve_colors_and_reader_input_in_stream_order() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(TerminalSourceInput::Bytes(
                b"x\x1b]11;rgb:1234/5678/9abc\x1b\\\x1b[200~pasted\x1b[201~\x1b]10;broken\x07y"
                    .to_vec(),
            ))
            .unwrap();
        drop(sender);
        let mut events = TerminalEvents::from_source(source).unwrap();
        let mut output = Vec::new();

        events
            .request_probe(&mut output, &TerminalFacts::default())
            .unwrap();

        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Colors(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(18, 86, 154)),
            ))
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Paste("pasted".into()))
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )))
        );
        assert!(events.next().await.is_none());
        assert!(!output.is_empty());
    }

    #[tokio::test]
    async fn color_reply_arriving_after_startup_remains_semantic_input() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();

        events
            .request_probe(&mut Vec::new(), &TerminalFacts::default())
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(
                b"\x1b]11;rgb:ffff/ffff/ffff\x07".to_vec(),
            ))
            .unwrap();
        drop(sender);
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Colors(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(255, 255, 255)),
            ))
        );
    }

    #[tokio::test]
    async fn source_boundaries_after_escape_preserve_protocol_and_key_sequences() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender.send(TerminalSourceInput::Bytes(vec![0x1b])).unwrap();
        sender
            .send(TerminalSourceInput::Bytes(
                b"]11;rgb:ffff/ffff/ffff\x07\x1b".to_vec(),
            ))
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(b"[?997;2n\x1b".to_vec()))
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(b"[A".to_vec()))
            .unwrap();
        drop(sender);
        let mut events = TerminalEvents::from_source(source).unwrap();

        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Colors(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(255, 255, 255)),
            ))
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Reprobe
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
        );
    }

    #[tokio::test]
    async fn idle_timeout_resolves_a_standalone_escape_key() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender.send(TerminalSourceInput::Bytes(vec![0x1b])).unwrap();
        let mut events = TerminalEvents::from_source(source).unwrap();

        let event = tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .expect("escape ambiguity timeout")
            .unwrap()
            .unwrap();

        assert_eq!(
            event,
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
        );
    }

    #[tokio::test]
    async fn idle_timeout_replays_an_ambiguous_color_prefix_when_no_query_is_outstanding() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(TerminalSourceInput::Bytes(b"\x1b]10".to_vec()))
            .unwrap();
        let mut events = TerminalEvents::from_source(source).unwrap();

        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT,
            )))
        );
        for expected in ['1', '0'] {
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char(expected),
                    KeyModifiers::NONE,
                )))
            );
        }
    }

    #[tokio::test]
    async fn color_reply_completes_after_the_response_window_expires() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();
        events
            .request_probe(&mut Vec::new(), &TerminalFacts::default())
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(
                b"\x1b]11;rgb:ffff/ffff".to_vec(),
            ))
            .unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .expect("the response window closes")
                .unwrap()
                .unwrap(),
            TerminalInput::Graphics(GraphicsReply::Expired),
            "the window closed on the probe rather than turning an incomplete reply into reader input"
        );
        sender
            .send(TerminalSourceInput::Bytes(b"/ffff\x07x".to_vec()))
            .unwrap();
        drop(sender);

        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Colors(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(255, 255, 255)),
            ))
        );
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
        );
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn incomplete_expected_color_response_expires_without_becoming_keys() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(TerminalSourceInput::Bytes(
                b"\x1b]11;rgb:ffff/ffff".to_vec(),
            ))
            .unwrap();
        let mut events = TerminalEvents::from_source(source).unwrap();
        events.expect_replies();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.next())
                .await
                .is_err(),
            "an incomplete protocol reply became reader input"
        );
        sender
            .send(TerminalSourceInput::Bytes(b"ignored\x07x".to_vec()))
            .unwrap();
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
        );
    }

    #[tokio::test]
    async fn startup_graphics_replies_and_reader_input_keep_stream_order() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(TerminalSourceInput::Bytes(joined(&[
                b"x",
                KITTY_OK,
                WEZTERM_VERSION,
                b"\x1b[6;20;10t",
                SIXEL_ATTRIBUTES,
                b"y",
            ])))
            .unwrap();
        drop(sender);
        let mut events = TerminalEvents::from_source(source).unwrap();
        events
            .request_probe(&mut Vec::new(), &TerminalFacts::default())
            .unwrap();

        let mut observed = Vec::new();
        while let Some(input) = events.next().await {
            observed.push(input.unwrap());
        }

        assert_eq!(
            observed,
            vec![
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                TerminalInput::Graphics(GraphicsReply::Kitty),
                TerminalInput::Graphics(GraphicsReply::Version(
                    "WezTerm 20240203-110809-5046fc22".to_owned()
                )),
                TerminalInput::Graphics(GraphicsReply::CellSize(CELL)),
                TerminalInput::Graphics(GraphicsReply::DeviceAttributes { sixel: true }),
                key(KeyCode::Char('y'), KeyModifiers::NONE),
            ]
        );
    }

    #[tokio::test]
    async fn a_probe_the_terminal_ignores_expires_after_the_readers_own_input() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();
        events
            .request_probe(&mut Vec::new(), &TerminalFacts::default())
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(b"hi\x1b[A".to_vec()))
            .unwrap();

        let mut facts = measured();
        let mut observed = Vec::new();
        while observed.len() < 4 {
            let input = tokio::time::timeout(Duration::from_millis(100), events.next())
                .await
                .expect("the response window closes")
                .unwrap()
                .unwrap();
            if let TerminalInput::Graphics(reply) = &input {
                facts.merge_graphics(reply);
            }
            observed.push(input);
        }

        assert_eq!(
            observed,
            vec![
                key(KeyCode::Char('h'), KeyModifiers::NONE),
                key(KeyCode::Char('i'), KeyModifiers::NONE),
                key(KeyCode::Up, KeyModifiers::NONE),
                TerminalInput::Graphics(GraphicsReply::Expired),
            ]
        );
        assert_eq!(facts.graphics, None);
        drop(sender);
    }

    #[tokio::test]
    async fn device_attributes_settle_the_probe_so_it_never_expires() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();
        events
            .request_probe(&mut Vec::new(), &TerminalFacts::default())
            .unwrap();
        sender
            .send(TerminalSourceInput::Bytes(PLAIN_ATTRIBUTES.to_vec()))
            .unwrap();

        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Graphics(GraphicsReply::DeviceAttributes { sixel: false })
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.next())
                .await
                .is_err(),
            "a settled probe has nothing left to expire"
        );
        drop(sender);
    }

    #[cfg(windows)]
    #[test]
    fn windows_records_forward_vt_bytes_repeats_and_resize_in_order() {
        use windows_sys::Win32::System::Console::{INPUT_RECORD, MOUSE_EVENT};

        let mut ignored_mouse = INPUT_RECORD::default();
        ignored_mouse.EventType = MOUSE_EVENT as u16;
        let records = [
            windows_key(0xf0, 1, true),
            windows_key(0x9f, 1, true),
            windows_key(0x98, 1, true),
            windows_key(0x80, 1, true),
            windows_key(b'x', 3, true),
            windows_key(b'z', 1, false),
            ignored_mouse,
            windows_resize(120, 40),
            windows_key(b'y', 1, true),
        ];

        let mut output = super::decode_windows_input_records(&records).into_iter();
        assert!(matches!(
            output.next(),
            Some(TerminalSourceInput::Bytes(bytes))
                if bytes == [0xf0, 0x9f, 0x98, 0x80, b'x', b'x', b'x']
        ));
        assert!(matches!(
            output.next(),
            Some(TerminalSourceInput::Event(InputEvent::Resize(120, 40)))
        ));
        assert!(matches!(
            output.next(),
            Some(TerminalSourceInput::Bytes(bytes)) if bytes == [b'y']
        ));
        assert!(output.next().is_none());
    }
}
