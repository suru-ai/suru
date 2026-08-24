//! Telling one emoji from anything else.
//!
//! An Emoji arrives from a Model, which means it arrives as whatever the Model
//! felt like writing. Suru stores it only when it is a single emoji, so no
//! surface presenting Sessions ever draws mojibake where an Emoji belongs — and
//! deciding that is the whole of this module's business, kept apart from what
//! any particular Emoji is for.

const ZERO_WIDTH_JOINER: char = '\u{200D}';
const VARIATION_SELECTOR_16: char = '\u{FE0F}';
const COMBINING_KEYCAP: char = '\u{20E3}';

/// A Model-authored Emoji as Suru stores it — trimmed of the space around it —
/// or `None` when what arrived is not a single emoji.
pub(super) fn single_emoji(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    is_single_emoji(trimmed).then(|| trimmed.to_owned())
}

/// Whether `text` is exactly one emoji: one base character, optionally
/// presented as emoji, optionally toned, optionally joined to further such
/// characters — plus the two sequences that spell an emoji out of characters
/// that are not emoji on their own, a flag's regional-indicator pair and a
/// keycap.
fn is_single_emoji(text: &str) -> bool {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.is_empty() {
        return false;
    }
    if is_regional_indicator(characters[0]) {
        return characters.len() == 2 && is_regional_indicator(characters[1]);
    }
    if characters.len() >= 2 && characters[characters.len() - 1] == COMBINING_KEYCAP {
        return matches!(characters[0], '0'..='9' | '#' | '*')
            && characters[1..characters.len() - 1]
                .iter()
                .all(|character| *character == VARIATION_SELECTOR_16);
    }

    let mut index = 0;
    loop {
        if !characters.get(index).copied().is_some_and(is_emoji_base) {
            return false;
        }
        index += 1;
        if characters.get(index) == Some(&VARIATION_SELECTOR_16) {
            index += 1;
        }
        if characters.get(index).copied().is_some_and(is_skin_tone) {
            index += 1;
        }
        match characters.get(index) {
            None => return true,
            Some(&ZERO_WIDTH_JOINER) => index += 1,
            Some(_) => return false,
        }
    }
}

fn is_regional_indicator(character: char) -> bool {
    matches!(u32::from(character), 0x1F1E6..=0x1F1FF)
}

fn is_skin_tone(character: char) -> bool {
    matches!(u32::from(character), 0x1F3FB..=0x1F3FF)
}

/// Whether a character can stand as an emoji on its own. Read off the Unicode
/// blocks emoji are drawn from rather than off a property table, because Suru
/// only has to tell an emoji from a letter, a digit, or punctuation — the cases
/// a Model actually answers with when it answers wrongly.
fn is_emoji_base(character: char) -> bool {
    matches!(
        u32::from(character),
        0x203C | 0x2049
            | 0x2122
            | 0x2139
            | 0x2194..=0x21AA
            | 0x231A..=0x231B
            | 0x2328
            | 0x23CF
            | 0x23E9..=0x23FA
            | 0x24C2
            | 0x25AA..=0x25FE
            | 0x2600..=0x27BF
            | 0x2934..=0x2935
            | 0x2B00..=0x2BFF
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0x1F000..=0x1FAFF
    )
}

#[cfg(test)]
mod tests {
    use super::single_emoji;

    #[test]
    fn one_emoji_is_kept_and_anything_else_is_discarded() {
        for single in [
            "\u{1F680}",                                                    // rocket
            "\u{2728}",                                                     // sparkles
            "\u{2764}\u{FE0F}",                    // heart, emoji-presented
            "\u{1F44D}\u{1F3FD}",                  // thumbs up, toned
            "\u{1F469}\u{200D}\u{1F4BB}",          // woman technologist
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}", // woman technologist, toned
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",  // rainbow flag: presented, then joined
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}", // a family, joined four ways
            "\u{1F1EF}\u{1F1F5}",                                           // flag of Japan
            "3\u{FE0F}\u{20E3}",                                            // keycap three
        ] {
            assert_eq!(
                single_emoji(single),
                Some(single.to_owned()),
                "{single:?} is a single emoji"
            );
        }
        for rejected in [
            "",
            "A",
            ":)",
            "rocket",
            "\u{1F680}\u{1F525}",
            "\u{1F680} and more",
            "\u{FE0F}",
        ] {
            assert_eq!(
                single_emoji(rejected),
                None,
                "{rejected:?} is not a single emoji"
            );
        }
    }

    #[test]
    fn an_emoji_keeps_none_of_the_space_around_it() {
        assert_eq!(
            single_emoji("  \u{1F680} "),
            Some("\u{1F680}".to_owned()),
            "an Emoji is stored without the space a Model padded it with"
        );
    }
}
