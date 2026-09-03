//! Terminal capabilities and colors observed before the Application starts.

use std::{
    io,
    time::{Duration, Instant},
};

const TERMINAL_COLOR_QUERY_SUFFIX: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
const MAX_TERMINAL_PROBE_BYTES: usize = 64 * 1024;

pub(crate) const DEFAULT_TERMINAL_PROBE_BUDGET: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalProbeOutcome {
    pub(crate) probe: Option<TerminalColorProbe>,
    pub(crate) late_response: LateTerminalColorResponse,
}

/// An OSC color response whose prefix the bounded raw-input probe consumed.
/// The terminal event boundary carries this parser state forward so a suffix
/// arriving after the budget is discarded rather than becoming key events.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum LateTerminalColorResponse {
    /// No query was sent (or all replies arrived), so every input event is user input.
    #[default]
    Disabled,
    /// A query was sent and a whole late response may still arrive.
    AwaitingResponse,
    Escape,
    Osc {
        payload: String,
        escape_terminator: bool,
    },
}

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

/// The colors a startup terminal probe reported.
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
}

impl Default for TerminalFacts {
    fn default() -> Self {
        Self::unprobed(false)
    }
}

trait TerminalProbeIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;

    fn flush(&mut self) -> io::Result<()>;

    fn read(&mut self, bytes: &mut [u8], timeout: Duration) -> io::Result<usize>;
}

fn probe_terminal_with(
    terminal: &mut impl TerminalProbeIo,
    budget: Duration,
) -> TerminalProbeOutcome {
    let mut query = Vec::new();
    for index in 0..16 {
        query.extend_from_slice(format!("\x1b]4;{index};?\x1b\\").as_bytes());
    }
    query.extend_from_slice(TERMINAL_COLOR_QUERY_SUFFIX);
    let mut written = 0;
    while written < query.len() {
        match terminal.write(&query[written..]) {
            Ok(0) | Err(_) => {
                return if written == 0 {
                    TerminalProbeOutcome::default()
                } else {
                    TerminalProbeOutcome {
                        probe: None,
                        late_response: LateTerminalColorResponse::AwaitingResponse,
                    }
                };
            }
            Ok(count) => written += count,
        }
    }
    if terminal.flush().is_err() {
        return TerminalProbeOutcome {
            probe: None,
            late_response: LateTerminalColorResponse::AwaitingResponse,
        };
    }

    let deadline = Instant::now() + budget;
    let mut response = Vec::new();
    while response.len() < MAX_TERMINAL_PROBE_BYTES {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let mut chunk = [0; 1024];
        let Ok(count) = terminal.read(&mut chunk, deadline.saturating_duration_since(now)) else {
            break;
        };
        if count == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..count]);
        let parsed = parse_terminal_probe(&response);
        if parsed.complete {
            return TerminalProbeOutcome {
                probe: parsed.probe,
                late_response: LateTerminalColorResponse::Disabled,
            };
        }
    }
    TerminalProbeOutcome {
        probe: parse_terminal_probe(&response).probe,
        late_response: incomplete_terminal_color_response(&response),
    }
}

struct ParsedTerminalProbe {
    probe: Option<TerminalColorProbe>,
    complete: bool,
}

#[derive(Default)]
struct SeenTerminalReplies {
    palette: [bool; 16],
    foreground: bool,
    background: bool,
}

fn parse_terminal_probe(bytes: &[u8]) -> ParsedTerminalProbe {
    let mut probe = TerminalColorProbe::new([None; 16], None, None);
    let mut seen = SeenTerminalReplies::default();
    let mut found = false;
    let mut cursor = 0;
    while let Some(start) = bytes[cursor..]
        .windows(2)
        .position(|window| window == b"\x1b]")
        .map(|start| cursor + start + 2)
    {
        let Some((end, terminator_len)) = osc_end(&bytes[start..]) else {
            break;
        };
        if let Ok(payload) = std::str::from_utf8(&bytes[start..start + end]) {
            found |= apply_osc_color(payload, &mut probe, &mut seen);
        }
        cursor = start + end + terminator_len;
    }
    ParsedTerminalProbe {
        probe: found.then_some(probe),
        complete: seen.palette.into_iter().all(|seen| seen) && seen.foreground && seen.background,
    }
}

fn osc_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x07 => return Some((index, 1)),
            0x1b if bytes.get(index + 1) == Some(&b'\\') => return Some((index, 2)),
            _ => index += 1,
        }
    }
    None
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

pub(crate) fn is_terminal_color_response(payload: &str) -> bool {
    apply_osc_color(
        payload,
        &mut TerminalColorProbe::new([None; 16], None, None),
        &mut SeenTerminalReplies::default(),
    )
}

pub(crate) fn could_be_terminal_color_response(payload: &str) -> bool {
    ["4;", "10;", "11;"]
        .iter()
        .any(|prefix| prefix.starts_with(payload) || payload.starts_with(prefix))
}

fn incomplete_terminal_color_response(bytes: &[u8]) -> LateTerminalColorResponse {
    let Some(start) = bytes.windows(2).rposition(|window| window == b"\x1b]") else {
        return if bytes.last() == Some(&0x1b) {
            LateTerminalColorResponse::Escape
        } else {
            LateTerminalColorResponse::AwaitingResponse
        };
    };
    let rest = &bytes[start + 2..];
    if let Some((end, terminator_len)) = osc_end(rest) {
        return if rest[end + terminator_len..] == [0x1b] {
            LateTerminalColorResponse::Escape
        } else {
            LateTerminalColorResponse::AwaitingResponse
        };
    }
    let escape_terminator = rest.last() == Some(&0x1b);
    let payload = if escape_terminator {
        &rest[..rest.len() - 1]
    } else {
        rest
    };
    let Ok(payload) = std::str::from_utf8(payload) else {
        return LateTerminalColorResponse::AwaitingResponse;
    };
    if could_be_terminal_color_response(payload) {
        LateTerminalColorResponse::Osc {
            payload: payload.to_owned(),
            escape_terminator,
        }
    } else {
        LateTerminalColorResponse::AwaitingResponse
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

#[cfg(unix)]
pub(crate) fn probe_terminal_colors(budget: Duration) -> TerminalProbeOutcome {
    UnixTerminalProbe::open().map_or_else(
        |_| TerminalProbeOutcome::default(),
        |mut terminal| probe_terminal_with(&mut terminal, budget),
    )
}

#[cfg(not(unix))]
pub(crate) fn probe_terminal_colors(_budget: Duration) -> TerminalProbeOutcome {
    TerminalProbeOutcome::default()
}

#[cfg(unix)]
struct UnixTerminalProbe {
    reader: std::fs::File,
    writer: std::fs::File,
}

#[cfg(unix)]
impl UnixTerminalProbe {
    fn open() -> io::Result<Self> {
        use std::os::fd::FromRawFd;

        fn duplicate(fd: libc::c_int) -> io::Result<std::fs::File> {
            let duplicate = unsafe { libc::dup(fd) };
            if duplicate == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { std::fs::File::from_raw_fd(duplicate) })
        }

        Ok(Self {
            reader: duplicate(libc::STDIN_FILENO)?,
            writer: duplicate(libc::STDOUT_FILENO)?,
        })
    }
}

#[cfg(unix)]
impl TerminalProbeIo for UnixTerminalProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        use std::io::Write;

        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        use std::io::Write;

        self.writer.flush()
    }

    fn read(&mut self, bytes: &mut [u8], timeout: Duration) -> io::Result<usize> {
        use std::{io::Read, os::fd::AsRawFd};

        let deadline = Instant::now() + timeout;
        let mut descriptor = libc::pollfd {
            fd: self.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(0);
            }
            let remaining = deadline.saturating_duration_since(now);
            let milliseconds = remaining
                .as_millis()
                .saturating_add(u128::from(
                    !remaining.subsec_nanos().is_multiple_of(1_000_000),
                ))
                .min(libc::c_int::MAX as u128) as libc::c_int;
            let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if result > 0 {
                return self.reader.read(bytes);
            }
            if result == 0 {
                return Ok(0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io, time::Duration};

    use super::{
        LateTerminalColorResponse, TerminalColor, TerminalColorProbe, TerminalProbeIo,
        probe_terminal_with,
    };

    #[derive(Default)]
    struct ScriptedTerminal {
        written: Vec<u8>,
        reads: VecDeque<Vec<u8>>,
        read_calls: usize,
        write_error_after: Option<usize>,
        flush_error: bool,
        read_error: bool,
    }

    impl TerminalProbeIo for ScriptedTerminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let allowed = self.write_error_after.map_or(bytes.len(), |limit| {
                limit.saturating_sub(self.written.len())
            });
            let count = allowed.min(bytes.len());
            if count == 0 {
                return Err(io::Error::other("scripted write failure"));
            }
            self.written.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.flush_error {
                Err(io::Error::other("scripted flush failure"))
            } else {
                Ok(())
            }
        }

        fn read(&mut self, bytes: &mut [u8], _timeout: Duration) -> io::Result<usize> {
            self.read_calls += 1;
            if self.read_error {
                return Err(io::Error::other("scripted read failure"));
            }
            let Some(read) = self.reads.pop_front() else {
                return Ok(0);
            };
            bytes[..read.len()].copy_from_slice(&read);
            Ok(read.len())
        }
    }

    #[test]
    fn startup_probe_writes_the_sixteen_palette_and_default_color_queries() {
        let mut terminal = ScriptedTerminal::default();

        let _ = probe_terminal_with(&mut terminal, Duration::ZERO);

        let mut expected = Vec::new();
        for index in 0..16 {
            expected.extend_from_slice(format!("\x1b]4;{index};?\x1b\\").as_bytes());
        }
        expected.extend_from_slice(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
        assert_eq!(terminal.written, expected);
    }

    #[test]
    fn startup_probe_parses_bel_and_st_terminated_terminal_colors() {
        let mut terminal = ScriptedTerminal {
            reads: VecDeque::from([b"\x1b]4;4;rgb:f/8/0\x07\x1b]10;rgb:aaaa/bbbb/cccc\x07\x1b]11;rgb:1234/5678/9abc\x1b\\".to_vec()]),
            ..ScriptedTerminal::default()
        };

        let probe = probe_terminal_with(&mut terminal, Duration::from_secs(1))
            .probe
            .expect("parse terminal facts");
        let mut palette = [None; 16];
        palette[4] = Some(TerminalColor::new(255, 136, 0));
        assert_eq!(
            probe,
            TerminalColorProbe::new(
                palette,
                Some(TerminalColor::new(170, 187, 204)),
                Some(TerminalColor::new(18, 86, 154)),
            )
        );
        assert!(terminal.reads.is_empty(), "no reply bytes reach crossterm");
    }

    #[test]
    fn startup_probe_returns_none_when_its_injected_budget_has_elapsed() {
        let mut terminal = ScriptedTerminal {
            reads: VecDeque::from([b"\x1b]11;rgb:ffff/ffff/ffff\x07".to_vec()]),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::ZERO);

        assert_eq!(outcome.probe, None);
        assert_eq!(
            outcome.late_response,
            LateTerminalColorResponse::AwaitingResponse
        );
        assert_eq!(terminal.reads.len(), 1, "the elapsed probe reads no bytes");
    }

    #[test]
    fn startup_probe_finishes_as_soon_as_every_reply_arrives() {
        let mut reply = Vec::new();
        for index in 0..16 {
            reply.extend_from_slice(
                format!("\x1b]4;{index};rgb:{index:x}/{index:x}/{index:x}\x07").as_bytes(),
            );
        }
        reply
            .extend_from_slice(b"\x1b]10;rgb:eeee/eeee/eeee\x1b\\\x1b]11;rgb:1111/1111/1111\x1b\\");
        let mut terminal = ScriptedTerminal {
            reads: VecDeque::from([reply]),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert!(outcome.probe.is_some());
        assert_eq!(terminal.read_calls, 1, "a complete reply ends the probe");
    }

    #[test]
    fn startup_probe_carries_an_incomplete_reply_across_the_budget_boundary() {
        let mut terminal = ScriptedTerminal {
            reads: VecDeque::from([b"\x1b]11;rgb:ff".to_vec()]),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert_eq!(outcome.probe, None);
        assert_eq!(
            outcome.late_response,
            LateTerminalColorResponse::Osc {
                payload: "11;rgb:ff".to_owned(),
                escape_terminator: false,
            }
        );
    }

    #[test]
    fn startup_probe_carries_a_trailing_escape_after_a_complete_reply() {
        let mut terminal = ScriptedTerminal {
            reads: VecDeque::from([b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b".to_vec()]),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert!(outcome.probe.is_some());
        assert_eq!(outcome.late_response, LateTerminalColorResponse::Escape);
    }

    #[test]
    fn a_failure_before_any_query_byte_disables_late_reply_filtering() {
        let mut terminal = ScriptedTerminal {
            write_error_after: Some(0),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert_eq!(outcome, super::TerminalProbeOutcome::default());
    }

    #[test]
    fn a_partial_query_write_failure_keeps_late_reply_filtering_armed() {
        let mut terminal = ScriptedTerminal {
            write_error_after: Some(8),
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert_eq!(terminal.written.len(), 8);
        assert_eq!(outcome.probe, None);
        assert_eq!(
            outcome.late_response,
            LateTerminalColorResponse::AwaitingResponse
        );
    }

    #[test]
    fn a_read_failure_after_the_query_keeps_late_reply_filtering_armed() {
        let mut terminal = ScriptedTerminal {
            read_error: true,
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert!(!terminal.written.is_empty());
        assert_eq!(outcome.probe, None);
        assert_eq!(
            outcome.late_response,
            LateTerminalColorResponse::AwaitingResponse
        );
    }

    #[test]
    fn a_flush_failure_after_the_query_keeps_late_reply_filtering_armed() {
        let mut terminal = ScriptedTerminal {
            flush_error: true,
            ..ScriptedTerminal::default()
        };

        let outcome = probe_terminal_with(&mut terminal, Duration::from_secs(1));

        assert!(!terminal.written.is_empty());
        assert_eq!(outcome.probe, None);
        assert_eq!(
            outcome.late_response,
            LateTerminalColorResponse::AwaitingResponse
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platforms_neither_probe_nor_filter_terminal_input() {
        assert_eq!(
            super::probe_terminal_colors(Duration::from_secs(1)),
            super::TerminalProbeOutcome::default()
        );
    }
}
