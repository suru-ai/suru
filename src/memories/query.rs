//! The words an Agent searches Memories by, read into a query the full-text
//! index runs as it stands.
//!
//! An Agent writes whatever it likes, so nothing it writes reaches the index's
//! own query language as written. Four things are read: words; double
//! quotes, which keep the words between them together as a phrase, a quote
//! left open running to the end; `OR`, written in capitals between two terms,
//! which lets either be found; and `AND`, written in capitals, which says what
//! is so anyway. A word in double quotes is always a word, so `"AND"` searches
//! for `and`. Every other mark — the index's other operators, a column
//! filter's colon, a prefix's star, parentheses — only separates words, and a
//! word written with marks inside it, such as `friday's` or `e-mail`, is the
//! phrase of the words they separate, as the index reads such text in a
//! Memory. Every word reaches the index inside double quotes, so nothing in a
//! query is ever read as syntax, and a query fails only by holding nothing to
//! search for, or too much.
//!
//! A word is a run of letters and digits, each with whatever accents are
//! written after it as marks of their own, since the index folds a decomposed
//! accent into its letter as it folds a composed one: `naïve` written either
//! way is one word, the index's own, and never two.

use unicode_segmentation::UnicodeSegmentation;

use super::MemoryRefusal;

/// The most words one query may hold.
pub(crate) const MAX_QUERY_WORDS: usize = 32;

/// A query the full-text index runs: every term found, or one of the terms
/// `OR` joins, each term a quoted phrase of one or more words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MatchQuery(String);

/// One thing a query was read as.
enum Token {
    /// A phrase: words that must stand together, in this order.
    Term(Vec<String>),
    Or,
}

impl MatchQuery {
    /// The query `written` asks for, or `None` where it is nothing but
    /// whitespace and so asks for no words at all. Refused where it holds no
    /// word, only operators and marks, or more than [`MAX_QUERY_WORDS`].
    pub(crate) fn read(written: &str) -> Result<Option<Self>, MemoryRefusal> {
        if written.trim().is_empty() {
            return Ok(None);
        }
        // Each group must be found; within one, any of its phrases.
        let mut groups = Vec::<Vec<Vec<String>>>::new();
        let mut joining = false;
        for token in tokens(written) {
            match token {
                // An OR with no term before it joins nothing.
                Token::Or => joining = !groups.is_empty(),
                Token::Term(words) => {
                    match groups.last_mut() {
                        Some(group) if joining => group.push(words),
                        _ => groups.push(vec![words]),
                    }
                    joining = false;
                }
            }
        }
        let words = groups.iter().flatten().map(Vec::len).sum::<usize>();
        if words == 0 {
            return Err(MemoryRefusal::NothingToSearch);
        }
        if words > MAX_QUERY_WORDS {
            return Err(MemoryRefusal::TooManyWords { words });
        }
        let expression = groups
            .iter()
            .map(|group| match group.as_slice() {
                [phrase] => quoted(phrase),
                alternatives => format!(
                    "({})",
                    alternatives
                        .iter()
                        .map(|phrase| quoted(phrase))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                ),
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        Ok(Some(Self(expression)))
    }

    /// The query in the index's own language.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// `phrase` as the index reads a string: quoted, any quote within doubled.
fn quoted(phrase: &[String]) -> String {
    format!("\"{}\"", phrase.join(" ").replace('"', "\"\""))
}

/// What `written` was read as, in order.
fn tokens(written: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = written;
    while !rest.is_empty() {
        if let Some(quoted) = rest.strip_prefix('"') {
            let (phrase, after) = quoted.split_once('"').unwrap_or((quoted, ""));
            tokens.extend(term(phrase));
            rest = after;
            continue;
        }
        let end = rest
            .find(|character: char| character == '"' || character.is_whitespace())
            .unwrap_or(rest.len());
        let (chunk, after) = rest.split_at(end);
        match chunk {
            "OR" => tokens.push(Token::Or),
            "AND" => {}
            chunk => tokens.extend(term(chunk)),
        }
        // Whitespace separates; a quote opens the next phrase.
        rest = after
            .trim_start_matches(|character: char| character != '"' && character.is_whitespace());
    }
    tokens
}

/// The phrase of the words `text` holds, or nothing where it holds none. A
/// word is a run of characters as a reader sees them — each with the marks
/// written after it — that begin with a letter or a digit.
fn term(text: &str) -> Option<Token> {
    let mut words = Vec::new();
    let mut word = String::new();
    for character in text.graphemes(true) {
        if character.starts_with(char::is_alphanumeric) {
            word.push_str(character);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    (!words.is_empty()).then_some(Token::Term(words))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(written: &str) -> Result<Option<String>, MemoryRefusal> {
        MatchQuery::read(written).map(|query| query.map(|query| query.as_str().to_owned()))
    }

    #[test]
    fn words_must_each_be_found_unless_or_joins_them() {
        assert_eq!(
            read("review dependency"),
            Ok(Some("\"review\" AND \"dependency\"".to_owned()))
        );
        assert_eq!(
            read("release OR café ci"),
            Ok(Some("(\"release\" OR \"café\") AND \"ci\"".to_owned()))
        );
        assert_eq!(
            read("a OR b OR c"),
            Ok(Some("(\"a\" OR \"b\" OR \"c\")".to_owned()))
        );
        assert_eq!(
            read("rust AND ci"),
            Ok(Some("\"rust\" AND \"ci\"".to_owned())),
            "AND in capitals says what is so anyway"
        );
        assert_eq!(
            read("or and not"),
            Ok(Some("\"or\" AND \"and\" AND \"not\"".to_owned())),
            "only OR and AND in capitals are read as operators"
        );
    }

    #[test]
    fn quoted_words_stand_together() {
        assert_eq!(
            read("\"full suite\" ci"),
            Ok(Some("\"full suite\" AND \"ci\"".to_owned()))
        );
        assert_eq!(
            read("\"full suite"),
            Ok(Some("\"full suite\"".to_owned())),
            "to the end"
        );
        assert_eq!(
            read("x\"y z\"w"),
            Ok(Some("\"x\" AND \"y z\" AND \"w\"".to_owned()))
        );
        for operator in ["OR", "AND"] {
            assert_eq!(
                read(&format!("\"{operator}\"")),
                Ok(Some(format!("\"{operator}\""))),
                "a quoted {operator} is a word"
            );
        }
    }

    #[test]
    fn an_accent_written_as_a_mark_of_its_own_stays_in_its_word() {
        for (written, read_as) in [
            ("nai\u{308}ve", "\"nai\u{308}ve\""),
            ("re\u{301}sume\u{301}", "\"re\u{301}sume\u{301}\""),
            ("naïve résumé", "\"naïve\" AND \"résumé\""),
            ("\u{301}cafe", "\"cafe\""),
        ] {
            assert_eq!(read(written), Ok(Some(read_as.to_owned())), "{written:?}");
        }
    }

    #[test]
    fn every_other_mark_only_separates_words() {
        for (written, read_as) in [
            ("deploy*", "\"deploy\""),
            ("-friday", "\"friday\""),
            ("NEAR(friday deploy)", "\"NEAR friday\" AND \"deploy\""),
            ("title:secret", "\"title secret\""),
            ("friday's", "\"friday s\""),
            (
                "'; DROP TABLE memories; --",
                "\"DROP\" AND \"TABLE\" AND \"memories\"",
            ),
            ("friday\u{0}", "\"friday\""),
            ("🦀 friday", "\"friday\""),
            ("東京の会議", "\"東京の会議\""),
            ("OR friday OR", "\"friday\""),
            ("\"\" friday", "\"friday\""),
        ] {
            assert_eq!(read(written), Ok(Some(read_as.to_owned())), "{written:?}");
        }
    }

    #[test]
    fn a_query_of_nothing_but_whitespace_asks_for_no_words_and_one_of_marks_is_refused() {
        for written in ["", "   ", "\n\t"] {
            assert_eq!(read(written), Ok(None), "{written:?}");
        }
        for written in [
            "!!!",
            "\"\"",
            "\" \"",
            "OR",
            "AND",
            "AND OR AND",
            "***",
            "🦀",
            "\"",
            "()",
            "\u{301}",
        ] {
            assert_eq!(
                read(written),
                Err(MemoryRefusal::NothingToSearch),
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_query_holds_a_bounded_number_of_words() {
        let most = vec!["word"; MAX_QUERY_WORDS].join(" ");
        assert!(read(&most).is_ok());
        assert_eq!(
            read(&format!("{most} \"one more\"")),
            Err(MemoryRefusal::TooManyWords {
                words: MAX_QUERY_WORDS + 2
            })
        );
    }
}
