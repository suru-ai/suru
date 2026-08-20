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

/// Stateful normalization for provider text that may arrive in arbitrary
/// streaming chunks. The emitted form is safe to persist as transcript data:
/// printable text, newlines, complete SGR sequences, and complete OSC 8
/// hyperlinks only.
#[derive(Debug, Default)]
pub(crate) struct ProviderTextNormalizer {
    scanner: AnsiScanner,
    remaining_chars: Option<usize>,
    style_may_be_active: bool,
    hyperlink_active: bool,
    truncated: bool,
    overwrite_line: Option<OverwriteLine>,
}

#[derive(Debug, Default)]
struct OverwriteLine {
    content: String,
    formatting_prefix: String,
    overwrite_pending: bool,
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
        self.overwrite_line
            .as_mut()
            .map(|line| std::mem::take(&mut line.content))
            .unwrap_or_default()
    }

    fn push_text(&mut self, text: &str, normalized: &mut String) {
        let Some(remaining) = self.remaining_chars.as_mut() else {
            self.push_visible(text, normalized);
            return;
        };
        let mut characters = text.chars();
        let visible = characters.by_ref().take(*remaining).collect::<String>();
        let emitted = text.chars().count().min(*remaining);
        *remaining -= emitted;
        self.push_visible(&visible, normalized);
        if characters.next().is_some() {
            self.mark_truncated(normalized);
        }
    }

    fn push_sgr(&mut self, sequence: &str, normalized: &mut String) {
        if self.push_formatting_sequence(sequence, normalized) {
            update_style_state(sequence, &mut self.style_may_be_active);
        }
    }

    fn push_osc8(&mut self, hyperlink: &Osc8, normalized: &mut String) {
        if self.push_formatting_sequence(hyperlink.sequence(), normalized) {
            self.hyperlink_active = hyperlink.target().is_some();
        }
    }

    fn push_formatting_sequence(&mut self, sequence: &str, normalized: &mut String) -> bool {
        let sequence_chars = sequence.chars().count();
        if let Some(remaining) = self.remaining_chars.as_mut() {
            if sequence_chars > *remaining {
                self.mark_truncated(normalized);
                return false;
            }
            *remaining -= sequence_chars;
        }
        if let Some(line) = self.overwrite_line.as_mut() {
            line.content.push_str(sequence);
            line.formatting_prefix.push_str(sequence);
        } else {
            normalized.push_str(sequence);
        }
        true
    }

    fn push_newline(&mut self, normalized: &mut String) {
        if let Some(remaining) = self.remaining_chars.as_mut() {
            if *remaining == 0 {
                self.mark_truncated(normalized);
                return;
            }
            *remaining -= 1;
        }
        if let Some(line) = self.overwrite_line.as_mut() {
            line.content.push('\n');
            normalized.push_str(&line.content);
            line.content.clear();
            line.formatting_prefix.clear();
            line.overwrite_pending = false;
        } else {
            normalized.push('\n');
        }
    }

    fn overwrite_line(&mut self) {
        if let Some(line) = self.overwrite_line.as_mut() {
            line.overwrite_pending = true;
        }
    }

    fn push_visible(&mut self, text: &str, normalized: &mut String) {
        if let Some(line) = self.overwrite_line.as_mut() {
            if line.overwrite_pending && !text.is_empty() {
                line.content.clone_from(&line.formatting_prefix);
                line.overwrite_pending = false;
            }
            line.content.push_str(text);
        } else {
            normalized.push_str(text);
        }
    }

    fn mark_truncated(&mut self, normalized: &mut String) {
        self.truncated = true;
        normalized.push_str(&self.finish());
        if self.hyperlink_active {
            normalized.push_str("\x1b]8;;\x1b\\");
            self.hyperlink_active = false;
        }
        if self.style_may_be_active {
            normalized.push_str("\x1b[0m");
            self.style_may_be_active = false;
        }
    }
}

pub(crate) fn normalize_provider_text(text: &str) -> String {
    ProviderTextNormalizer::default().push(text)
}

fn update_style_state(sequence: &str, style_may_be_active: &mut bool) {
    let Some(parameters) = sequence
        .strip_prefix("\x1b[")
        .and_then(|sequence| sequence.strip_suffix('m'))
    else {
        return;
    };
    for parameter in parameters.split(';') {
        let parameter = parameter.split(':').next().unwrap_or_default();
        let parameter = if parameter.is_empty() {
            Some(0)
        } else {
            parameter.parse::<u16>().ok()
        };
        match parameter {
            Some(0) => *style_may_be_active = false,
            Some(_) | None => *style_may_be_active = true,
        }
    }
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
}
