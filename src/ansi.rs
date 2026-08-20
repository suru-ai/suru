#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Fragment {
    Text(String),
    Sgr(String),
    Csi(String),
    Osc(String),
    Dcs(String),
    Apc(String),
    Escape(String),
    Control(char),
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape(String),
    Csi(String),
    Osc {
        sequence: String,
        escape_pending: bool,
    },
    Dcs {
        sequence: String,
        escape_pending: bool,
    },
    Apc {
        sequence: String,
        escape_pending: bool,
    },
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
                    self.state = State::Escape(character.to_string());
                }
                State::Ground if character.is_control() || character == '\x7f' => {
                    if !text.is_empty() {
                        fragments.push(Fragment::Text(std::mem::take(&mut text)));
                    }
                    fragments.push(Fragment::Control(character));
                }
                State::Ground => text.push(character),
                State::Escape(mut sequence) => {
                    sequence.push(character);
                    match character {
                        '[' => self.state = State::Csi(sequence),
                        ']' => {
                            self.state = State::Osc {
                                sequence,
                                escape_pending: false,
                            };
                        }
                        'P' => {
                            self.state = State::Dcs {
                                sequence,
                                escape_pending: false,
                            };
                        }
                        '_' => {
                            self.state = State::Apc {
                                sequence,
                                escape_pending: false,
                            };
                        }
                        '\u{20}'..='\u{2f}' => self.state = State::Escape(sequence),
                        '\u{30}'..='\u{7e}' => fragments.push(Fragment::Escape(sequence)),
                        _ => {}
                    }
                }
                State::Csi(mut sequence) => {
                    sequence.push(character);
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        if character == 'm' {
                            fragments.push(Fragment::Sgr(sequence));
                        } else {
                            fragments.push(Fragment::Csi(sequence));
                        }
                    } else {
                        self.state = State::Csi(sequence);
                    }
                }
                State::Osc {
                    mut sequence,
                    escape_pending,
                } => {
                    sequence.push(character);
                    if character == '\x07' || (escape_pending && character == '\\') {
                        fragments.push(Fragment::Osc(sequence));
                    } else {
                        self.state = State::Osc {
                            sequence,
                            escape_pending: character == '\x1b',
                        };
                    }
                }
                State::Dcs {
                    mut sequence,
                    escape_pending,
                } => {
                    sequence.push(character);
                    if escape_pending && character == '\\' {
                        fragments.push(Fragment::Dcs(sequence));
                    } else {
                        self.state = State::Dcs {
                            sequence,
                            escape_pending: character == '\x1b',
                        };
                    }
                }
                State::Apc {
                    mut sequence,
                    escape_pending,
                } => {
                    sequence.push(character);
                    if escape_pending && character == '\\' {
                        fragments.push(Fragment::Apc(sequence));
                    } else {
                        self.state = State::Apc {
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

#[cfg(test)]
mod tests {
    use super::{AnsiScanner, Fragment};

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
}
