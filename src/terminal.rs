//! Terminal input and capabilities observed throughout the Application lifetime.

use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{Stream, StreamExt};

const TERMINAL_COLOR_QUERY_SUFFIX: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
pub(crate) const DEFAULT_TERMINAL_PROBE_BUDGET: Duration = Duration::from_millis(100);
const DEFAULT_TERMINAL_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
const DEFAULT_COLOR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
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

/// Everything the terminal told Suru that affects Theme resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFacts {
    pub probe: Option<TerminalColorProbe>,
    pub truecolor: bool,
}

impl TerminalFacts {
    pub const fn new(probe: Option<TerminalColorProbe>, truecolor: bool) -> Self {
        Self { probe, truecolor }
    }

    pub const fn unprobed(truecolor: bool) -> Self {
        Self::new(None, truecolor)
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
    Reprobe,
}

/// Incrementally turns the terminal's raw byte stream into semantic input.
/// Partial escape sequences remain buffered across calls.
#[derive(Debug, Default)]
pub(crate) struct TerminalInputParser {
    parser: termina::Parser,
    state: RawInputState,
    in_paste: bool,
    expecting_colors: bool,
}

impl TerminalInputParser {
    pub(crate) fn parse(&mut self, bytes: &[u8], maybe_more: bool) -> Vec<TerminalInput> {
        const MAX_OSC_BYTES: usize = 64 * 1024;

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
                RawInputState::Escape if byte == b']' && !self.in_paste => {
                    self.state = RawInputState::Osc {
                        raw: b"\x1b]".to_vec(),
                        payload: Vec::new(),
                        escape_terminator: false,
                    };
                }
                RawInputState::Escape if byte == b'[' => {
                    self.state = RawInputState::Csi {
                        raw: b"\x1b[".to_vec(),
                    };
                }
                RawInputState::Escape => {
                    ordinary.extend_from_slice(&[0x1b, byte]);
                    self.state = RawInputState::Ground;
                }
                RawInputState::Csi { raw } => {
                    raw.push(byte);
                    let paste_start = BRACKETED_PASTE_START.starts_with(raw);
                    let paste_end = BRACKETED_PASTE_END.starts_with(raw);
                    if raw == BRACKETED_PASTE_START || raw == BRACKETED_PASTE_END {
                        self.in_paste = raw == BRACKETED_PASTE_START;
                        ordinary.append(raw);
                        self.state = RawInputState::Ground;
                    } else if !paste_start && !paste_end {
                        ordinary.append(raw);
                        self.state = RawInputState::Ground;
                    }
                }
                RawInputState::Osc {
                    raw,
                    payload,
                    escape_terminator,
                } => {
                    raw.push(byte);
                    if *escape_terminator {
                        if byte == b'\\' {
                            let payload = std::str::from_utf8(payload).ok();
                            if let Some(colors) = payload.and_then(parse_color_reply) {
                                output.push(TerminalInput::Colors(colors));
                            }
                            self.state = RawInputState::Ground;
                        } else {
                            payload.extend_from_slice(&[0x1b, byte]);
                            *escape_terminator = false;
                        }
                    } else if byte == 0x07 {
                        let payload = std::str::from_utf8(payload).ok();
                        if let Some(colors) = payload.and_then(parse_color_reply) {
                            output.push(TerminalInput::Colors(colors));
                        }
                        self.state = RawInputState::Ground;
                    } else if byte == 0x1b {
                        *escape_terminator = true;
                    } else {
                        payload.push(byte);
                        let candidate = std::str::from_utf8(payload)
                            .is_ok_and(could_be_terminal_color_response);
                        if raw.len() > MAX_OSC_BYTES {
                            self.state = RawInputState::DiscardOsc {
                                escape_terminator: false,
                            };
                        } else if !candidate {
                            push_alt_bracket(&mut output);
                            ordinary.extend_from_slice(payload);
                            self.state = RawInputState::Ground;
                        }
                    }
                }
                RawInputState::DiscardOsc { escape_terminator } => {
                    if *escape_terminator && byte == b'\\' || byte == 0x07 {
                        self.state = RawInputState::Ground;
                    } else {
                        *escape_terminator = byte == 0x1b;
                    }
                }
            }
        }
        if !maybe_more {
            match &mut self.state {
                RawInputState::Escape => {
                    ordinary.push(0x1b);
                    self.state = RawInputState::Ground;
                }
                RawInputState::Csi { raw } => {
                    ordinary.append(raw);
                    self.state = RawInputState::Ground;
                }
                // Only ambiguous Alt-] input expires. Once an OSC color prefix
                // is recognized, read through its terminator even if it is late.
                RawInputState::Osc { payload, .. }
                    if !self.expecting_colors && !is_confirmed_color_response(payload) =>
                {
                    push_alt_bracket(&mut output);
                    ordinary.extend_from_slice(payload);
                    self.state = RawInputState::Ground;
                }
                RawInputState::Ground
                | RawInputState::Osc { .. }
                | RawInputState::DiscardOsc { .. } => {}
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

    fn expect_colors(&mut self) {
        self.expecting_colors = true;
    }

    fn stop_expecting_colors(&mut self) {
        // The query window only governs ambiguous prefixes. A recognized
        // reply may still finish later and must retain its accumulated colors.
        self.expecting_colors = false;
    }
}

fn push_alt_bracket(output: &mut Vec<TerminalInput>) {
    output.push(TerminalInput::Event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(']'),
            crossterm::event::KeyModifiers::ALT,
        ),
    )));
}

fn is_confirmed_color_response(payload: &[u8]) -> bool {
    [b"4;".as_slice(), b"10;".as_slice(), b"11;".as_slice()]
        .iter()
        .any(|prefix| payload.starts_with(prefix))
}

#[derive(Debug, Default)]
enum RawInputState {
    #[default]
    Ground,
    Escape,
    Csi {
        raw: Vec<u8>,
    },
    Osc {
        raw: Vec<u8>,
        payload: Vec<u8>,
        escape_terminator: bool,
    },
    DiscardOsc {
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
    color_response_timeout: Duration,
    color_response_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
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
            DEFAULT_COLOR_RESPONSE_TIMEOUT,
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
        color_response_timeout: Duration,
    ) -> io::Result<Self> {
        Ok(Self {
            _reader: reader,
            source,
            parser: TerminalInputParser::default(),
            ready: VecDeque::new(),
            source_done: false,
            sequence_timeout,
            sequence_idle: None,
            color_response_timeout,
            color_response_deadline: None,
            #[cfg(unix)]
            resize: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?,
        })
    }

    /// Sends the startup color query and consumes replies for at most `budget`.
    /// Reader input that arrives during the probe is retained in order for the
    /// main run loop.
    pub(crate) async fn probe_colors(
        &mut self,
        output: &mut impl io::Write,
        budget: Duration,
    ) -> io::Result<Option<TerminalColorProbe>> {
        request_terminal_colors(output)?;
        self.expect_color_responses();
        let deadline = tokio::time::Instant::now() + budget;
        let mut deferred = VecDeque::new();
        let mut facts = TerminalFacts::unprobed(false);
        loop {
            let item = match tokio::time::timeout_at(deadline, self.next()).await {
                Ok(Some(item)) => item,
                Ok(None) | Err(_) => break,
            };
            match item {
                Ok(TerminalInput::Colors(update)) => {
                    facts.merge_probe(update);
                    if facts.probe.is_some_and(terminal_probe_complete) {
                        break;
                    }
                }
                other => deferred.push_back(other),
            }
        }
        while let Some(item) = deferred.pop_back() {
            self.ready.push_front(item);
        }
        Ok(facts.probe)
    }

    fn expect_color_responses(&mut self) {
        self.parser.expect_colors();
        self.color_response_deadline =
            Some(Box::pin(tokio::time::sleep(self.color_response_timeout)));
    }

    fn queue_parsed(&mut self, parsed: Vec<TerminalInput>) {
        let refresh_expectation = parsed.iter().any(|input| {
            matches!(input, TerminalInput::Reprobe)
                || (self.color_response_deadline.is_some()
                    && matches!(input, TerminalInput::Colors(_)))
        });
        self.ready.extend(parsed.into_iter().map(Ok));
        if refresh_expectation {
            self.expect_color_responses();
        }
    }
}

pub(crate) fn request_terminal_colors(output: &mut impl io::Write) -> io::Result<()> {
    for index in 0..16 {
        write!(output, "\x1b]4;{index};?\x1b\\")?;
    }
    output.write_all(TERMINAL_COLOR_QUERY_SUFFIX)?;
    output.flush()
}

fn terminal_probe_complete(probe: TerminalColorProbe) -> bool {
    probe.palette.into_iter().all(|color| color.is_some())
        && probe.foreground.is_some()
        && probe.background.is_some()
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
                    this.color_response_deadline = None;
                    this.parser.stop_expecting_colors();
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
                .color_response_deadline
                .as_mut()
                .is_some_and(|timeout| timeout.as_mut().poll(context).is_ready())
            {
                this.color_response_deadline = None;
                this.parser.stop_expecting_colors();
                let parsed = this.parser.parse(&[], false);
                this.queue_parsed(parsed);
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

pub(crate) fn could_be_terminal_color_response(payload: &str) -> bool {
    ["4;", "10;", "11;"]
        .iter()
        .any(|prefix| prefix.starts_with(payload) || payload.starts_with(prefix))
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
        TerminalColor, TerminalColorProbe, TerminalEvents, TerminalInput, TerminalInputParser,
        TerminalSourceInput, request_terminal_colors,
    };
    use crossterm::event::{
        Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

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
        parser.expect_colors();
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
        );
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
    async fn startup_probe_consumes_colors_but_preserves_reader_input() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(TerminalSourceInput::Bytes(
                b"x\x1b]11;rgb:1234/5678/9abc\x1b\\".to_vec(),
            ))
            .unwrap();
        drop(sender);
        let mut events = TerminalEvents::from_source(source).unwrap();
        let mut output = Vec::new();

        let probe = events
            .probe_colors(&mut output, Duration::from_millis(10))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(probe.background, Some(TerminalColor::new(18, 86, 154)));
        assert_eq!(
            events.next().await.unwrap().unwrap(),
            TerminalInput::Event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
        );
        assert!(!output.is_empty());
    }

    #[tokio::test]
    async fn color_reply_arriving_after_the_startup_budget_remains_semantic_input() {
        use futures_util::StreamExt as _;

        let (sender, source) = tokio::sync::mpsc::unbounded_channel();
        let mut events = TerminalEvents::from_source(source).unwrap();

        assert_eq!(
            events
                .probe_colors(&mut Vec::new(), Duration::ZERO)
                .await
                .unwrap(),
            None
        );
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
        assert_eq!(
            events
                .probe_colors(&mut Vec::new(), Duration::ZERO)
                .await
                .unwrap(),
            None
        );
        sender
            .send(TerminalSourceInput::Bytes(
                b"\x1b]11;rgb:ffff/ffff".to_vec(),
            ))
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.next())
                .await
                .is_err(),
            "an incomplete protocol reply became reader input"
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
        events.expect_color_responses();

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
