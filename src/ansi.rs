#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Fragment {
    Text(String),
    Sgr(String),
    Csi(String),
    Osc8(Osc8),
    Osc(String),
    Dcs(String),
    Apc(String),
    Escape(String),
    Control(char),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Osc8 {
    sequence: String,
    target: Option<String>,
}

impl Osc8 {
    fn parse(sequence: String) -> Result<Self, String> {
        let Some(payload) = sequence.strip_prefix("\x1b]").and_then(|sequence| {
            sequence
                .strip_suffix('\x07')
                .or_else(|| sequence.strip_suffix("\x1b\\"))
        }) else {
            return Err(sequence);
        };
        let mut fields = payload.splitn(3, ';');
        if fields.next() != Some("8") || fields.next().is_none() {
            return Err(sequence);
        }
        let Some(target) = fields.next() else {
            return Err(sequence);
        };
        if payload.chars().any(char::is_control) {
            return Err(sequence);
        }
        let target = (!target.is_empty()).then(|| target.to_owned());
        Ok(Self { sequence, target })
    }

    pub(crate) fn sequence(&self) -> &str {
        &self.sequence
    }

    pub(crate) fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape {
        sequence: String,
        has_intermediate: bool,
    },
    Csi {
        sequence: String,
        sgr_candidate: bool,
    },
    String {
        kind: StringKind,
        sequence: String,
        escape_pending: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum StringKind {
    Osc,
    Dcs,
    Apc,
}

impl StringKind {
    fn is_terminated_by(self, character: char, escape_pending: bool) -> bool {
        (matches!(self, Self::Osc) && character == '\x07') || (escape_pending && character == '\\')
    }

    fn into_fragment(self, sequence: String) -> Fragment {
        match self {
            Self::Osc => Osc8::parse(sequence).map_or_else(Fragment::Osc, Fragment::Osc8),
            Self::Dcs => Fragment::Dcs(sequence),
            Self::Apc => Fragment::Apc(sequence),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct AnsiScanner {
    state: State,
}

impl AnsiScanner {
    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<Fragment> {
        let mut fragments = Vec::new();
        let mut text = String::new();
        for character in chunk.chars() {
            match std::mem::take(&mut self.state) {
                State::Ground if character == '\x1b' => {
                    if !text.is_empty() {
                        fragments.push(Fragment::Text(std::mem::take(&mut text)));
                    }
                    self.state = State::Escape {
                        sequence: character.to_string(),
                        has_intermediate: false,
                    };
                }
                State::Ground if character.is_control() || character == '\x7f' => {
                    if !text.is_empty() {
                        fragments.push(Fragment::Text(std::mem::take(&mut text)));
                    }
                    fragments.push(Fragment::Control(character));
                }
                State::Ground => text.push(character),
                State::Escape {
                    mut sequence,
                    has_intermediate,
                } => {
                    sequence.push(character);
                    match (has_intermediate, character) {
                        (false, '[') => {
                            self.state = State::Csi {
                                sequence,
                                sgr_candidate: true,
                            };
                        }
                        (false, ']') => {
                            self.state = State::String {
                                kind: StringKind::Osc,
                                sequence,
                                escape_pending: false,
                            };
                        }
                        (false, 'P') => {
                            self.state = State::String {
                                kind: StringKind::Dcs,
                                sequence,
                                escape_pending: false,
                            };
                        }
                        (false, '_') => {
                            self.state = State::String {
                                kind: StringKind::Apc,
                                sequence,
                                escape_pending: false,
                            };
                        }
                        (_, '\u{20}'..='\u{2f}') => {
                            self.state = State::Escape {
                                sequence,
                                has_intermediate: true,
                            };
                        }
                        (_, '\u{30}'..='\u{7e}') => {
                            fragments.push(Fragment::Escape(sequence));
                        }
                        _ => {}
                    }
                }
                State::Csi {
                    mut sequence,
                    sgr_candidate,
                } => {
                    sequence.push(character);
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        if character == 'm' && sgr_candidate {
                            fragments.push(Fragment::Sgr(sequence));
                        } else {
                            fragments.push(Fragment::Csi(sequence));
                        }
                    } else {
                        self.state = State::Csi {
                            sequence,
                            sgr_candidate: sgr_candidate
                                && matches!(character, '0'..='9' | ';' | ':'),
                        };
                    }
                }
                State::String {
                    kind,
                    mut sequence,
                    escape_pending,
                } => {
                    sequence.push(character);
                    if kind.is_terminated_by(character, escape_pending) {
                        fragments.push(kind.into_fragment(sequence));
                    } else {
                        self.state = State::String {
                            kind,
                            sequence,
                            escape_pending: character == '\x1b',
                        };
                    }
                }
            }
        }
        if !text.is_empty() {
            fragments.push(Fragment::Text(text));
        }
        fragments
    }
}

/// The most SGR attributes tracked at once, comfortably above the number of
/// attributes Suru recognizes.
const MAX_TRACKED_SGR_ATTRIBUTES: usize = 32;

const SGR_RESET: &str = "\x1b[0m";
const HYPERLINK_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Stateful normalization for provider text that may arrive in arbitrary
/// streaming chunks. The emitted form is safe to persist as transcript data:
/// printable text, newlines, complete SGR sequences, and complete OSC 8
/// hyperlinks only.
#[derive(Debug, Default)]
pub(crate) struct ProviderTextNormalizer {
    scanner: AnsiScanner,
    remaining_chars: Option<usize>,
    formatting: Formatting,
    truncated: bool,
    overwrite_line: Option<OverwriteLine>,
}

/// A line withheld from the emitted output until a newline, or the end of the
/// stream, commits it. Withholding it lets a carriage return discard the frame
/// it overwrites before that frame is charged against the remaining budget.
#[derive(Debug, Default)]
struct OverwriteLine {
    stored: StoredLine,
    /// The formatting a reader of the stored output carries into this line.
    committed_formatting: Formatting,
    /// The formatting a reader of the stored output carries at the end of what
    /// this line has stored, which lags the stream whenever the line holds a
    /// formatting sequence back.
    stored_formatting: Formatting,
    overwrite_pending: bool,
}

/// What a line has stored so far, and whether the budget already cut it short.
#[derive(Debug, Default)]
struct StoredLine {
    content: String,
    chars: usize,
    overflowed: bool,
}

impl OverwriteLine {
    /// Readies the line to store text under `formatting`. A pending overwrite
    /// discards the frame it replaces first; either way the line then stores
    /// the sequences that carry a reader from the formatting the stored output
    /// last put in effect to the formatting this text is under.
    fn prepare_for_text(&mut self, formatting: &Formatting, limit: Option<usize>) {
        if self.overwrite_pending {
            self.stored = StoredLine::default();
            self.stored_formatting = self.committed_formatting.clone();
            self.overwrite_pending = false;
        }
        if self.stored_formatting == *formatting {
            return;
        }
        let transition = formatting.transition_from(&self.stored_formatting);
        self.stored.push_sequence(&transition, limit);
        self.stored_formatting = formatting.clone();
    }

    /// Empties the line, returning what it stored. A pending overwrite is
    /// spent along with it.
    fn take(&mut self) -> StoredLine {
        self.overwrite_pending = false;
        std::mem::take(&mut self.stored)
    }
}

impl StoredLine {
    /// Appends a formatting sequence whole or not at all, since a partially
    /// stored escape sequence would not be safe to persist.
    fn push_sequence(&mut self, sequence: &str, limit: Option<usize>) {
        if self.overflowed {
            return;
        }
        let chars = sequence.chars().count();
        if limit.is_some_and(|limit| self.chars + chars > limit) {
            self.overflowed = true;
            return;
        }
        self.content.push_str(sequence);
        self.chars += chars;
    }

    fn push_text(&mut self, text: &str, limit: Option<usize>) {
        if self.overflowed {
            return;
        }
        let (fitting, overflowed) =
            split_to_fit(text, limit.map(|limit| limit.saturating_sub(self.chars)));
        self.chars += fitting.chars().count();
        self.content.push_str(fitting);
        self.overflowed = overflowed;
    }
}

/// Splits `text` at `limit` characters, reporting whether anything was left
/// over. Splitting by character keeps a multi-byte character whole.
fn split_to_fit(text: &str, limit: Option<usize>) -> (&str, bool) {
    let Some(limit) = limit else {
        return (text, false);
    };
    match text.char_indices().nth(limit) {
        Some((offset, _)) => (&text[..offset], true),
        None => (text, false),
    }
}

impl ProviderTextNormalizer {
    pub(crate) fn with_max_chars(max_chars: usize) -> Self {
        Self {
            remaining_chars: Some(max_chars),
            ..Self::default()
        }
    }

    pub(crate) fn with_line_overwrite(max_chars: usize) -> Self {
        let mut normalizer = Self::with_max_chars(max_chars);
        normalizer.overwrite_line = Some(OverwriteLine::default());
        normalizer
    }

    pub(crate) fn push(&mut self, chunk: &str) -> String {
        if self.truncated {
            return String::new();
        }
        let mut normalized = String::with_capacity(chunk.len());
        for fragment in self.scanner.feed(chunk) {
            match fragment {
                Fragment::Text(text) => self.push_text(&text, &mut normalized),
                Fragment::Sgr(sequence) => self.push_sgr(&sequence, &mut normalized),
                Fragment::Osc8(hyperlink) => self.push_osc8(&hyperlink, &mut normalized),
                Fragment::Control('\t') => self.push_text("    ", &mut normalized),
                Fragment::Control('\n') => self.push_newline(&mut normalized),
                Fragment::Control('\r') => self.overwrite_line(),
                Fragment::Csi(_)
                | Fragment::Osc(_)
                | Fragment::Dcs(_)
                | Fragment::Apc(_)
                | Fragment::Escape(_)
                | Fragment::Control(_) => {}
            }
            if self.truncated {
                break;
            }
        }
        normalized
    }

    pub(crate) fn finish(&mut self) -> String {
        if self.truncated {
            return String::new();
        }
        let Some(line) = self.overwrite_line.as_mut() else {
            return String::new();
        };
        let stored = line.take();
        let mut normalized = stored.content;
        self.spend(stored.chars);
        if stored.overflowed {
            self.mark_truncated(&mut normalized);
        }
        normalized
    }

    fn push_text(&mut self, text: &str, normalized: &mut String) {
        let limit = self.remaining_chars;
        let formatting = &self.formatting;
        if let Some(line) = self.overwrite_line.as_mut() {
            if text.is_empty() {
                return;
            }
            line.prepare_for_text(formatting, limit);
            line.stored.push_text(text, limit);
            return;
        }
        let (visible, overflowed) = split_to_fit(text, limit);
        normalized.push_str(visible);
        self.spend(visible.chars().count());
        if overflowed {
            self.mark_truncated(normalized);
        }
    }

    fn push_sgr(&mut self, sequence: &str, normalized: &mut String) {
        if self.push_formatting_sequence(sequence, normalized) {
            self.formatting.apply_sgr(sequence);
            self.follow_stored_formatting();
        }
    }

    fn push_osc8(&mut self, hyperlink: &Osc8, normalized: &mut String) {
        if self.push_formatting_sequence(hyperlink.sequence(), normalized) {
            self.formatting.apply_osc8(hyperlink);
            self.follow_stored_formatting();
        }
    }

    /// Records that the stored line now carries the current formatting, which
    /// it does whenever a formatting sequence went into it verbatim.
    fn follow_stored_formatting(&mut self) {
        if let Some(line) = self.overwrite_line.as_mut()
            && !line.overwrite_pending
        {
            line.stored_formatting = self.formatting.clone();
        }
    }

    /// Stores a formatting sequence and reports whether the formatting state
    /// should follow it. An overwrite line always follows it: a sequence that
    /// no longer fits the line still applies to the frames that replace it.
    fn push_formatting_sequence(&mut self, sequence: &str, normalized: &mut String) -> bool {
        if let Some(line) = self.overwrite_line.as_mut() {
            if !line.overwrite_pending {
                line.stored.push_sequence(sequence, self.remaining_chars);
            }
            return true;
        }
        let sequence_chars = sequence.chars().count();
        if let Some(remaining) = self.remaining_chars.as_mut() {
            if sequence_chars > *remaining {
                self.mark_truncated(normalized);
                return false;
            }
            *remaining -= sequence_chars;
        }
        normalized.push_str(sequence);
        true
    }

    fn push_newline(&mut self, normalized: &mut String) {
        if self.overwrite_line.is_some() {
            self.commit_line(normalized);
            return;
        }
        let Some(remaining) = self.remaining_chars.as_mut() else {
            normalized.push('\n');
            return;
        };
        if *remaining == 0 {
            self.mark_truncated(normalized);
            return;
        }
        *remaining -= 1;
        normalized.push('\n');
    }

    /// Charges the surviving line against the budget and emits it, since a
    /// newline puts it beyond the reach of any further overwrite.
    fn commit_line(&mut self, normalized: &mut String) {
        let Some(line) = self.overwrite_line.as_mut() else {
            return;
        };
        let stored = line.take();
        normalized.push_str(&stored.content);
        let fits = !stored.overflowed
            && self
                .remaining_chars
                .is_none_or(|remaining| stored.chars < remaining);
        self.spend(stored.chars);
        if !fits {
            self.mark_truncated(normalized);
            return;
        }
        self.spend(1);
        normalized.push('\n');
        if let Some(line) = self.overwrite_line.as_mut() {
            line.committed_formatting = line.stored_formatting.clone();
        }
    }

    fn overwrite_line(&mut self) {
        if let Some(line) = self.overwrite_line.as_mut() {
            line.overwrite_pending = true;
        }
    }

    fn spend(&mut self, chars: usize) {
        if let Some(remaining) = self.remaining_chars.as_mut() {
            *remaining = remaining.saturating_sub(chars);
        }
    }

    fn mark_truncated(&mut self, normalized: &mut String) {
        self.truncated = true;
        normalized.push_str(&self.formatting.closers());
        self.formatting = Formatting::default();
    }
}

pub(crate) fn normalize_provider_text(text: &str) -> String {
    ProviderTextNormalizer::default().push(text)
}

/// The SGR style and OSC 8 hyperlink a terminal would have in effect at a
/// point in the stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Formatting {
    style: SgrState,
    hyperlink: Option<Osc8>,
}

impl Formatting {
    fn apply_sgr(&mut self, sequence: &str) {
        self.style.apply(sequence);
    }

    fn apply_osc8(&mut self, hyperlink: &Osc8) {
        self.hyperlink = hyperlink.target().is_some().then(|| hyperlink.to_owned());
    }

    /// The sequences that carry a reader of the stored stream from `previous`
    /// formatting to this one.
    fn transition_from(&self, previous: &Self) -> String {
        let mut transition = String::new();
        if self.style != previous.style {
            if !previous.style.is_empty() {
                transition.push_str(SGR_RESET);
            }
            transition.push_str(&self.style.render());
        }
        if self.hyperlink != previous.hyperlink {
            transition.push_str(
                self.hyperlink
                    .as_ref()
                    .map_or(HYPERLINK_CLOSE, Osc8::sequence),
            );
        }
        transition
    }

    /// The sequences that close whatever this formatting leaves open.
    fn closers(&self) -> String {
        let mut closers = String::new();
        if self.hyperlink.is_some() {
            closers.push_str(HYPERLINK_CLOSE);
        }
        if !self.style.is_empty() {
            closers.push_str(SGR_RESET);
        }
        closers
    }
}

/// The SGR attributes in effect, tracked per attribute so that a superseded
/// attribute is never replayed for text it does not apply to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SgrState {
    attributes: std::collections::BTreeMap<SgrAttribute, String>,
}

/// The attribute an SGR parameter addresses. Parameters addressing the same
/// attribute supersede one another; parameters Suru does not recognize keep
/// their own attribute so that they survive unchanged.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SgrAttribute {
    Intensity,
    Italic,
    Underline,
    Blink,
    Inverse,
    Conceal,
    Strike,
    Font,
    Spacing,
    Frame,
    Overline,
    Script,
    Foreground,
    Background,
    UnderlineColor,
    Unrecognized(String),
}

enum SgrEffect {
    ResetAll,
    Set(SgrAttribute),
    Clear(SgrAttribute),
    /// An indexed or RGB color, whose value continues into the parameters that
    /// follow it.
    ExtendedColor(SgrAttribute),
}

impl SgrState {
    fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }

    fn apply(&mut self, sequence: &str) {
        let Some(parameters) = sequence
            .strip_prefix("\x1b[")
            .and_then(|sequence| sequence.strip_suffix('m'))
        else {
            return;
        };
        let parameters = parameters.split(';').collect::<Vec<_>>();
        let mut index = 0;
        while index < parameters.len() {
            index += self.apply_parameter(&parameters, index);
        }
    }

    /// Applies the parameter at `index` and reports how many parameters it
    /// consumed.
    fn apply_parameter(&mut self, parameters: &[&str], index: usize) -> usize {
        let parameter = parameters[index];
        match sgr_effect(parameter) {
            SgrEffect::ResetAll => self.attributes.clear(),
            SgrEffect::Set(attribute) => self.set(attribute, parameter.to_owned()),
            SgrEffect::Clear(attribute) => {
                self.attributes.remove(&attribute);
            }
            SgrEffect::ExtendedColor(attribute) => {
                let length = extended_color_length(parameters, index);
                self.set(attribute, parameters[index..index + length].join(";"));
                return length;
            }
        }
        1
    }

    /// Records an attribute, dropping it once the tracked set is full. Only
    /// unrecognized parameters can fill it, and dropping one costs fidelity a
    /// stream of unrecognized parameters has already given up, whereas keeping
    /// every one would let such a stream grow this state without bound.
    fn set(&mut self, attribute: SgrAttribute, value: String) {
        if self.attributes.len() >= MAX_TRACKED_SGR_ATTRIBUTES
            && !self.attributes.contains_key(&attribute)
        {
            return;
        }
        self.attributes.insert(attribute, value);
    }

    fn render(&self) -> String {
        if self.attributes.is_empty() {
            return String::new();
        }
        let mut rendered = String::from("\x1b[");
        for (position, value) in self.attributes.values().enumerate() {
            if position > 0 {
                rendered.push(';');
            }
            rendered.push_str(value);
        }
        rendered.push('m');
        rendered
    }
}

fn sgr_effect(parameter: &str) -> SgrEffect {
    let colon_form = parameter.contains(':');
    let leading = parameter.split(':').next().unwrap_or_default();
    let code = if leading.is_empty() {
        Some(0)
    } else {
        leading.parse::<u16>().ok()
    };
    let Some(code) = code else {
        return SgrEffect::Set(SgrAttribute::Unrecognized(parameter.to_owned()));
    };
    match code {
        0 => SgrEffect::ResetAll,
        1 | 2 => SgrEffect::Set(SgrAttribute::Intensity),
        22 => SgrEffect::Clear(SgrAttribute::Intensity),
        3 | 20 => SgrEffect::Set(SgrAttribute::Italic),
        23 => SgrEffect::Clear(SgrAttribute::Italic),
        4 | 21 => SgrEffect::Set(SgrAttribute::Underline),
        24 => SgrEffect::Clear(SgrAttribute::Underline),
        5 | 6 => SgrEffect::Set(SgrAttribute::Blink),
        25 => SgrEffect::Clear(SgrAttribute::Blink),
        7 => SgrEffect::Set(SgrAttribute::Inverse),
        27 => SgrEffect::Clear(SgrAttribute::Inverse),
        8 => SgrEffect::Set(SgrAttribute::Conceal),
        28 => SgrEffect::Clear(SgrAttribute::Conceal),
        9 => SgrEffect::Set(SgrAttribute::Strike),
        29 => SgrEffect::Clear(SgrAttribute::Strike),
        10 => SgrEffect::Clear(SgrAttribute::Font),
        11..=19 => SgrEffect::Set(SgrAttribute::Font),
        26 => SgrEffect::Set(SgrAttribute::Spacing),
        50 => SgrEffect::Clear(SgrAttribute::Spacing),
        51 | 52 => SgrEffect::Set(SgrAttribute::Frame),
        54 => SgrEffect::Clear(SgrAttribute::Frame),
        53 => SgrEffect::Set(SgrAttribute::Overline),
        55 => SgrEffect::Clear(SgrAttribute::Overline),
        73 | 74 => SgrEffect::Set(SgrAttribute::Script),
        75 => SgrEffect::Clear(SgrAttribute::Script),
        30..=37 | 90..=97 => SgrEffect::Set(SgrAttribute::Foreground),
        38 if colon_form => SgrEffect::Set(SgrAttribute::Foreground),
        38 => SgrEffect::ExtendedColor(SgrAttribute::Foreground),
        39 => SgrEffect::Clear(SgrAttribute::Foreground),
        40..=47 | 100..=107 => SgrEffect::Set(SgrAttribute::Background),
        48 if colon_form => SgrEffect::Set(SgrAttribute::Background),
        48 => SgrEffect::ExtendedColor(SgrAttribute::Background),
        49 => SgrEffect::Clear(SgrAttribute::Background),
        58 if colon_form => SgrEffect::Set(SgrAttribute::UnderlineColor),
        58 => SgrEffect::ExtendedColor(SgrAttribute::UnderlineColor),
        59 => SgrEffect::Clear(SgrAttribute::UnderlineColor),
        _ => SgrEffect::Set(SgrAttribute::Unrecognized(parameter.to_owned())),
    }
}

/// How many parameters an extended color occupies: an index selector takes one
/// parameter and an RGB selector three, each after the selector itself.
fn extended_color_length(parameters: &[&str], index: usize) -> usize {
    let length = match parameters.get(index + 1).copied() {
        Some("5") => 3,
        Some("2") => 5,
        _ => 1,
    };
    length.min(parameters.len() - index)
}

#[cfg(test)]
mod tests {
    use super::{AnsiScanner, Fragment, ProviderTextNormalizer};

    #[test]
    fn classifies_sgr_separately_from_text() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("before\x1b[1;32mafter"),
            vec![
                Fragment::Text("before".to_owned()),
                Fragment::Sgr("\x1b[1;32m".to_owned()),
                Fragment::Text("after".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_non_sgr_csi_separately_from_sgr() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1b[2K\x1b[?25h\x1b[0m"),
            vec![
                Fragment::Csi("\x1b[2K".to_owned()),
                Fragment::Csi("\x1b[?25h".to_owned()),
                Fragment::Sgr("\x1b[0m".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_only_csi_m_with_sgr_parameters_as_sgr() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1b[1$m\x1b[>4;2m\x1b[38:2::1:2:3m"),
            vec![
                Fragment::Csi("\x1b[1$m".to_owned()),
                Fragment::Csi("\x1b[>4;2m".to_owned()),
                Fragment::Sgr("\x1b[38:2::1:2:3m".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_osc_with_bel_or_st_terminators() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1b]0;first\x07\x1b]2;second\x1b\\after"),
            vec![
                Fragment::Osc("\x1b]0;first\x07".to_owned()),
                Fragment::Osc("\x1b]2;second\x1b\\".to_owned()),
                Fragment::Text("after".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_dcs_and_apc_strings() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1bP1;2|payload\x1b\\\x1b_private\x1b\\after"),
            vec![
                Fragment::Dcs("\x1bP1;2|payload\x1b\\".to_owned()),
                Fragment::Apc("\x1b_private\x1b\\".to_owned()),
                Fragment::Text("after".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_escape_sequences_with_intermediates_and_finals() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1b(B\x1b7after"),
            vec![
                Fragment::Escape("\x1b(B".to_owned()),
                Fragment::Escape("\x1b7".to_owned()),
                Fragment::Text("after".to_owned()),
            ]
        );
    }

    #[test]
    fn escape_introducer_bytes_are_finals_after_an_intermediate() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("\x1b([\x1b(]\x1b(P\x1b(_after"),
            vec![
                Fragment::Escape("\x1b([".to_owned()),
                Fragment::Escape("\x1b(]".to_owned()),
                Fragment::Escape("\x1b(P".to_owned()),
                Fragment::Escape("\x1b(_".to_owned()),
                Fragment::Text("after".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_c0_controls_and_del_individually() {
        let mut scanner = AnsiScanner::default();

        assert_eq!(
            scanner.feed("a\tb\rc\x07d\0e\nf\x7f"),
            vec![
                Fragment::Text("a".to_owned()),
                Fragment::Control('\t'),
                Fragment::Text("b".to_owned()),
                Fragment::Control('\r'),
                Fragment::Text("c".to_owned()),
                Fragment::Control('\x07'),
                Fragment::Text("d".to_owned()),
                Fragment::Control('\0'),
                Fragment::Text("e".to_owned()),
                Fragment::Control('\n'),
                Fragment::Text("f".to_owned()),
                Fragment::Control('\x7f'),
            ]
        );
    }

    #[test]
    fn resumes_csi_and_osc_sequences_at_every_byte_boundary() {
        let cases = [
            ("\x1b[38;5;42m", Fragment::Sgr("\x1b[38;5;42m".to_owned())),
            (
                "\x1b]0;chunked title\x07",
                Fragment::Osc("\x1b]0;chunked title\x07".to_owned()),
            ),
            (
                "\x1b]0;chunked title\x1b\\",
                Fragment::Osc("\x1b]0;chunked title\x1b\\".to_owned()),
            ),
        ];

        for (input, expected) in cases {
            for split in 0..=input.len() {
                let mut scanner = AnsiScanner::default();
                let mut actual = scanner.feed(&input[..split]);
                actual.extend(scanner.feed(&input[split..]));
                assert_eq!(actual, vec![expected.clone()], "split at byte {split}");
            }
        }
    }

    #[test]
    fn provider_text_truncation_never_splits_sgr_and_resets_active_style() {
        let mut normalizer = ProviderTextNormalizer::with_max_chars(10);

        assert_eq!(
            normalizer.push("\x1b[31m1234\x1b[38;5;42m5678"),
            "\x1b[31m1234\x1b[0m"
        );
        assert_eq!(normalizer.push("ignored"), "");
    }

    #[test]
    fn provider_text_truncation_resets_styles_outside_the_rendered_subset() {
        let mut normalizer = ProviderTextNormalizer::with_max_chars(6);

        assert_eq!(normalizer.push("\x1b[53mxoverflow"), "\x1b[53mx\x1b[0m");
    }

    #[test]
    fn provider_text_preserves_complete_osc_8_hyperlinks() {
        let mut normalizer = ProviderTextNormalizer::default();

        assert_eq!(
            normalizer.push(concat!(
                "before ",
                "\x1b]8;id=docs;https://example.com/bel\x07BEL\x1b]8;;\x07 ",
                "\x1b]8;;https://example.com/st\x1b\\ST\x1b]8;;\x1b\\ after"
            )),
            concat!(
                "before ",
                "\x1b]8;id=docs;https://example.com/bel\x07BEL\x1b]8;;\x07 ",
                "\x1b]8;;https://example.com/st\x1b\\ST\x1b]8;;\x1b\\ after"
            )
        );
    }

    #[test]
    fn provider_text_truncation_closes_an_active_hyperlink() {
        let open = "\x1b]8;;https://example.com\x1b\\";
        let mut normalizer = ProviderTextNormalizer::with_max_chars(open.chars().count() + 3);

        assert_eq!(
            normalizer.push(&format!("{open}abcdef")),
            format!("{open}abc\x1b]8;;\x1b\\")
        );
        assert_eq!(normalizer.push("ignored"), "");
    }

    #[test]
    fn provider_text_resumes_osc_8_hyperlinks_at_every_byte_boundary() {
        let input = "\x1b]8;id=split;https://example.com\x1b\\linked\x1b]8;;\x1b\\";

        for split in 0..=input.len() {
            let mut normalizer = ProviderTextNormalizer::default();
            let mut actual = normalizer.push(&input[..split]);
            actual.push_str(&normalizer.push(&input[split..]));
            assert_eq!(actual, input, "split at byte {split}");
        }
    }

    #[test]
    fn provider_text_drops_malformed_and_non_8_osc_sequences() {
        let mut normalizer = ProviderTextNormalizer::default();

        assert_eq!(
            normalizer.push(concat!(
                "before",
                "\x1b]8;missing-target\x07",
                "\x1b]0;terminal title\x1b\\",
                "after"
            )),
            "beforeafter"
        );
    }

    #[test]
    fn provider_text_drops_an_unterminated_osc_8_across_deltas() {
        let mut normalizer = ProviderTextNormalizer::default();
        let mut actual = normalizer.push("before\x1b]8;;https://example");
        actual.push_str(&normalizer.push(".com/unterminated"));
        actual.push_str(&normalizer.finish());

        assert_eq!(actual, "before");
    }

    #[test]
    fn overwritten_line_frames_do_not_consume_the_stored_output_budget() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(64);
        let mut actual = String::new();

        for percent in 0..1_000 {
            actual.push_str(&normalizer.push(&format!("\r\x1b[31m{percent}% [####]")));
        }
        actual.push_str(&normalizer.push("\r\x1b[32mdone\x1b[0m\nnext line\n"));

        assert_eq!(actual, "\x1b[32mdone\x1b[0m\nnext line\n");
    }

    #[test]
    fn overwritten_line_keeps_only_the_style_applying_to_surviving_text() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push("\x1b[1;31mfirst frame\r\x1b[32msecond\n"),
            "\x1b[1;32msecond\n"
        );
    }

    #[test]
    fn overwritten_line_clears_styles_that_the_previous_line_left_active() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push("\x1b[1mbold\n\x1b[22mplain\rfinal\n"),
            "\x1b[1mbold\n\x1b[0mfinal\n"
        );
    }

    #[test]
    fn overwritten_line_reopens_and_closes_hyperlinks_around_surviving_text() {
        let open = "\x1b]8;;https://example.com\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push(&format!("{open}linked\rstill linked\n")),
            format!("{open}still linked\n")
        );
        assert_eq!(
            normalizer.push(&format!("{open}linked{close} plain\rafter\n")),
            format!("{close}after\n")
        );
    }

    #[test]
    fn overwritten_line_recovers_a_budget_spent_by_a_discarded_frame() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(16);

        assert_eq!(
            normalizer.push("a very long first frame\rshort\n"),
            "short\n"
        );
        assert_eq!(normalizer.push("kept\n"), "kept\n");
    }

    #[test]
    fn line_overwrite_charges_output_that_is_never_overwritten_as_before() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(10);

        assert_eq!(normalizer.push("\x1b[31m1234\x1b[38;5;42m5678"), "");
        assert_eq!(normalizer.finish(), "\x1b[31m1234\x1b[0m");
        assert_eq!(normalizer.push("ignored"), "");
    }

    #[test]
    fn line_overwrite_truncates_a_committed_line_that_exhausts_the_budget() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(6);

        assert_eq!(normalizer.push("\x1b[53mxoverflow\n"), "\x1b[53mx\x1b[0m");
        assert_eq!(normalizer.push("ignored"), "");
    }

    #[test]
    fn line_overwrite_keeps_a_line_that_exactly_fills_the_budget() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(6);

        assert_eq!(normalizer.push("123456"), "");
        assert_eq!(normalizer.finish(), "123456");
    }

    #[test]
    fn overwritten_line_bounds_the_styles_tracked_for_unrecognized_parameters() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        let unrecognized = (200..400)
            .map(|parameter| format!("\x1b[{parameter}m"))
            .collect::<String>();
        let rebuilt = normalizer.push(&format!("{unrecognized}frame\rsurvivor\n"));

        let styles = rebuilt
            .strip_suffix("survivor\n")
            .expect("the surviving text follows its restored styles");
        assert!(
            styles.chars().count() < 200,
            "restored styles stay bounded: {styles:?}"
        );
    }

    #[test]
    fn overwritten_line_drops_styles_that_a_later_frame_turned_off() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push("\x1b[4;53;38;5;42mstyled\r\x1b[24;55msurvivor\n"),
            "\x1b[38;5;42msurvivor\n"
        );
    }

    #[test]
    fn line_overwrite_stores_formatting_that_arrived_after_a_discarded_frame() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push("\x1b[31mred\r\x1b[32m\nplain\n"),
            "\x1b[31mred\n\x1b[0m\x1b[32mplain\n"
        );
    }

    #[test]
    fn line_overwrite_rebuilds_from_the_formatting_the_stored_line_ends_with() {
        let mut normalizer = ProviderTextNormalizer::with_line_overwrite(1_024);

        assert_eq!(
            normalizer.push("\x1b[31mred\r\x1b[32m\nnext\rX\n"),
            "\x1b[31mred\n\x1b[0m\x1b[32mX\n"
        );
    }
}
