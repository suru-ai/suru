//! Managed Worktree names: a few meaningful words of free text, shaped into
//! one fragment that is both a valid Git ref component and a portable
//! directory name on every platform. The fragment names the location beneath
//! the managed container and, behind [`BRANCH_PREFIX`], the branch.
use crate::protocol::{SkillInvocation, skill_marker_matches};

/// The fixed prefix of every branch Suru creates for a Managed Worktree.
pub const BRANCH_PREFIX: &str = "suru/";
/// How many names uniqueness tries — the fragment, then `-2` onwards — before
/// giving up rather than displacing whatever holds them.
pub const NAME_ATTEMPTS: usize = 32;
const MAX_WORDS: usize = 5;
const MAX_CHARS: usize = 32;
const FALLBACK: &str = "work";

/// English words that carry no meaning in a name: articles, pronouns,
/// auxiliaries, common connectives, and conversational filler. Apostrophes are
/// removed before matching, so contractions appear here without them.
const FILLER: &str = concat!(
    // Articles and determiners.
    "a an the this that these those some any ",
    // Pronouns and question words.
    "i me my mine myself we us our ours ourselves you your yours yourself he him his she her ",
    "hers it its itself they them their theirs something what which who how why where ",
    // Auxiliaries and their contractions.
    "am is are was were be been being do does did doing have has had having will would shall ",
    "should can could may might must gonna wanna im ive youre youve weve theyre theyve thats ",
    "theres whats hes shes ",
    // Connectives.
    "and or but so if then than when while with of to for in on at by from into as about ",
    "there here ",
    // Conversational filler.
    "lets let please pls currently just really actually basically simply maybe perhaps ",
    "probably kindly also very quite hey hi hello ok okay thanks thank um uh want need now",
);

/// Names Windows reserves for devices, which no file or directory may take.
const WINDOWS_DEVICES: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
    "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Shape arbitrary text into a name fragment: its ASCII alphanumeric words,
/// lowercased and joined by `-`, with [`FILLER`] left out. At most five words
/// and 32 characters are kept, dropping whole words rather than cutting one;
/// only a first word longer than the limit is cut to fit. Text with nothing
/// left names `work`.
///
/// Words are split wherever the text is neither alphanumeric nor an
/// apostrophe, and keep only their ASCII letters and digits, so every
/// fragment is a Git ref component and a portable directory name.
pub fn name_fragment(text: &str) -> String {
    let words = text
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '\u{2019}')
        .map(|word| {
            word.chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty() && !FILLER.split(' ').any(|filler| filler == word))
        .take(MAX_WORDS);
    let mut name = String::new();
    for word in words {
        if name.is_empty() {
            name.extend(word.chars().take(MAX_CHARS));
        } else if name.len() + 1 + word.len() <= MAX_CHARS {
            name.push('-');
            name.push_str(&word);
        } else {
            break;
        }
    }
    if name.is_empty() {
        name.push_str(FALLBACK);
    } else if WINDOWS_DEVICES.contains(&name.as_str()) {
        name.push('-');
        name.push_str(FALLBACK);
    }
    name
}

/// The names uniqueness tries for a fragment, in order: the fragment itself,
/// then `-2`, `-3`, … up to [`NAME_ATTEMPTS`] names in all.
pub fn numbered(fragment: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(fragment.to_owned())
        .chain((2..=NAME_ATTEMPTS).map(move |number| format!("{fragment}-{number}")))
}

/// A Prompt's text with the markers of its bound Skill Invocations removed by
/// their recorded spans, so a Skill's name never names the work. Unbound
/// `$tokens` carry no span and stay ordinary text. Each marker is replaced by
/// a space so the words around it stay apart.
pub(crate) fn without_skill_markers(
    text: &str,
    skill_invocations: &[SkillInvocation],
) -> Result<String, String> {
    let mut spans = Vec::with_capacity(skill_invocations.len());
    for invocation in skill_invocations {
        let span = invocation.marker.start as usize..invocation.marker.end as usize;
        if !text
            .get(span.clone())
            .is_some_and(|marker| skill_marker_matches(marker, &invocation.name))
        {
            return Err(format!(
                "Skill `{}` is not bound to its visible marker",
                invocation.name
            ));
        }
        spans.push(span);
    }
    spans.sort_by_key(|span| span.start);
    spans.dedup();
    let mut unmarked = String::with_capacity(text.len());
    let mut next = 0;
    for span in spans {
        if span.start < next {
            return Err("Skill Invocation markers overlap".to_owned());
        }
        unmarked.push_str(&text[next..span.start]);
        unmarked.push(' ');
        next = span.end;
    }
    unmarked.push_str(&text[next..]);
    Ok(unmarked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SkillId, SkillMarkerSpan};

    #[test]
    fn text_is_shaped_into_a_short_fragment_of_meaningful_words() {
        for (text, expected) in [
            // Filler removal.
            ("Fix the login bug", "fix-login-bug"),
            (
                "Please can you add a retry to the uploader",
                "add-retry-uploader",
            ),
            ("Let's rename the config loader", "rename-config-loader"),
            ("We're currently seeing flaky tests", "seeing-flaky-tests"),
            ("It’s broken when I click save", "broken-click-save"),
            // At most five words.
            (
                "alpha beta gamma delta epsilon zeta eta",
                "alpha-beta-gamma-delta-epsilon",
            ),
            // Whole-word truncation within 32 characters.
            (
                "when we create a new worktree currently we get an incredibly long name",
                "create-new-worktree-get",
            ),
            (
                "refactor authentication middleware thoroughly",
                "refactor-authentication",
            ),
            // A lone oversized word is cut to fit.
            (
                "Supercalifragilisticexpialidociousness everywhere",
                "supercalifragilisticexpialidocio",
            ),
            // Non-ASCII and emoji.
            ("👩🏽‍💻 ship the café menu 🚀", "ship-caf-menu"),
            ("修正 the parser", "parser"),
            // Repeated and mixed separators.
            ("  fix---the___parser //  now!!  ", "fix-parser"),
            ("A_very.Long~name / ", "long-name"),
            ("v2.1 release notes", "v2-1-release-notes"),
            // Unbound `$tokens` are ordinary text.
            ("$grill-with-docs explain", "grill-docs-explain"),
            // The `work` fallback.
            ("", "work"),
            ("   ", "work"),
            ("👩🏽‍💻 / ... ~ @{}", "work"),
            ("日本語だけ", "work"),
            ("Could you do this for me please?", "work"),
            // Windows device names are not portable directory names.
            ("the aux", "aux-work"),
            ("NUL", "nul-work"),
            ("con fig", "con-fig"),
        ] {
            let name = name_fragment(text);
            assert_eq!(name, expected, "{text:?}");
            assert!(name.len() <= MAX_CHARS + 1 + FALLBACK.len(), "{text:?}");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{text:?}"
            );
            assert!(!name.starts_with('-') && !name.ends_with('-') && !name.contains("--"));
        }
    }

    #[test]
    fn uniqueness_tries_the_bare_fragment_first_then_numbers_it() {
        let names = numbered("fix-parser").collect::<Vec<_>>();
        assert_eq!(names.len(), NAME_ATTEMPTS);
        assert_eq!(names[..3], ["fix-parser", "fix-parser-2", "fix-parser-3"]);
        assert_eq!(
            names.last().unwrap(),
            &format!("fix-parser-{NAME_ATTEMPTS}")
        );
    }

    fn bound(name: &str, start: u32, end: u32) -> SkillInvocation {
        SkillInvocation {
            skill_id: SkillId::new(name),
            name: name.to_owned(),
            scope: None,
            marker: SkillMarkerSpan { start, end },
        }
    }

    #[test]
    fn bound_skill_markers_are_removed_by_span_and_unbound_tokens_remain() {
        let text = "$grill-with-docs fix$it $unbound then $Grill-With-Docs";
        let unmarked = without_skill_markers(
            text,
            &[
                bound("grill-with-docs", 0, 16),
                bound("grill-with-docs", 38, 54),
            ],
        )
        .unwrap();
        assert_eq!(name_fragment(&unmarked), "fix-unbound");
        assert!(unmarked.contains("$unbound"));
        assert_eq!(
            without_skill_markers("$a$b", &[bound("a", 0, 2), bound("b", 2, 4)]).unwrap(),
            "  "
        );
    }

    #[test]
    fn a_marker_its_span_does_not_name_is_refused() {
        for invocation in [
            bound("review", 0, 99),
            bound("review", 1, 7),
            bound("explain", 0, 7),
        ] {
            assert!(without_skill_markers("$review this", &[invocation]).is_err());
        }
        assert!(without_skill_markers("é$review", &[bound("review", 1, 8)]).is_err());
    }
}
