//! Where a Prompt's text breaks across the composer's content rows.
//!
//! The caret, the composer block's height, and the styled lines a frame draws
//! all have to agree on the same wrap, so they all read it from one layout
//! rather than each re-deriving it. Rows break on whole words: a word that does
//! not fit the room left on a row moves to the next row entire, and only a word
//! too wide for a row of its own is split within.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// A Prompt's text laid out over content columns `width` wide, as the byte
/// range of each visual row.
pub(super) struct ComposerLayout<'a> {
    text: &'a str,
    width: usize,
    rows: Vec<Range<usize>>,
}

impl<'a> ComposerLayout<'a> {
    pub(super) fn new(text: &'a str, width: u16) -> Self {
        let width = usize::from(width.max(1));
        let mut rows = Vec::new();
        let mut row_start = 0_usize;
        let mut column = 0_usize;
        let mut offset = 0_usize;
        while let Some(character) = text[offset..].chars().next() {
            if character == '\n' {
                rows.push(row_start..offset);
                offset += character.len_utf8();
                row_start = offset;
                column = 0;
                continue;
            }
            if character.is_whitespace() {
                // Whitespace never starts a row of its own: the space that ends
                // a row stays with the row it ended, so a word moved down lands
                // at the left edge rather than a column in from it. A row can
                // run past `width` that way, and the columns past the edge are
                // blanks nobody sees.
                column += display_width(character);
                offset += character.len_utf8();
                continue;
            }
            let word = word_at(text, offset);
            let word_width = text[word.clone()].chars().map(display_width).sum::<usize>();
            if column > 0 && column + word_width > width {
                rows.push(row_start..offset);
                row_start = offset;
                column = 0;
            }
            if word_width <= width {
                column += word_width;
                offset = word.end;
                continue;
            }
            // The word is wider than a row of its own, so there is nowhere to
            // move it to: it fills rows character by character instead.
            for (character_offset, character) in text[word.clone()].char_indices() {
                let character_offset = word.start + character_offset;
                let character_width = display_width(character);
                if column > 0 && column + character_width > width {
                    rows.push(row_start..character_offset);
                    row_start = character_offset;
                    column = 0;
                }
                column += character_width;
            }
            offset = word.end;
        }
        rows.push(row_start..text.len());
        Self { text, width, rows }
    }

    /// Each visual row in order, as the byte offset it begins at and the text
    /// it draws.
    pub(super) fn rows(&self) -> impl Iterator<Item = (usize, &'a str)> + '_ {
        self.rows
            .iter()
            .map(|row| (row.start, &self.text[row.clone()]))
    }

    /// How many rows the text occupies.
    pub(super) fn row_count(&self) -> u16 {
        u16::try_from(self.rows.len()).unwrap_or(u16::MAX)
    }

    /// Where the caret sits for a byte offset into the text, as a row and a
    /// column. An offset that lands at a wrapped row's right edge spills to the
    /// start of the row below, which is where the next character typed goes;
    /// one at the right edge of a row a newline ended waits in that row's
    /// margin instead, since the row below holds a line of its own.
    pub(super) fn cursor_position(&self, cursor: usize) -> (u16, u16) {
        let row_index = self
            .rows
            .iter()
            .rposition(|row| row.start <= cursor)
            .unwrap_or(0);
        let row = &self.rows[row_index];
        let end = cursor.clamp(row.start, row.end);
        let column = self.text[row.start..end]
            .chars()
            .map(display_width)
            .sum::<usize>()
            .min(self.width);
        let row_index = u16::try_from(row_index).unwrap_or(u16::MAX);
        let newline_ended = self.text[row.end..].starts_with('\n');
        if column >= self.width && !newline_ended {
            (row_index.saturating_add(1), 0)
        } else {
            (row_index, u16::try_from(column).unwrap_or(u16::MAX))
        }
    }
}

/// The run of non-whitespace characters starting at `offset`.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let end = text[offset..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| offset + index)
        .unwrap_or(text.len());
    offset..end
}

fn display_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::ComposerLayout;

    fn rows(text: &str, width: u16) -> Vec<String> {
        ComposerLayout::new(text, width)
            .rows()
            .map(|(_, row)| row.to_owned())
            .collect()
    }

    #[test]
    fn a_word_that_does_not_fit_the_room_left_moves_to_the_next_row_whole() {
        assert_eq!(
            rows("hello wonderful world", 12),
            vec!["hello ", "wonderful ", "world"],
            "a word moves entire rather than leaving its first characters behind"
        );
    }

    #[test]
    fn a_word_too_wide_for_a_row_of_its_own_is_split_within() {
        assert_eq!(
            rows("hi abcdefghijkl", 6),
            vec!["hi ", "abcdef", "ghijkl"],
            "a word with nowhere to move to fills rows character by character"
        );
        assert_eq!(
            rows("abcdefghij", 4),
            vec!["abcd", "efgh", "ij"],
            "a lone oversized word starts on the first row it is already on"
        );
    }

    #[test]
    fn newlines_break_rows_and_a_trailing_newline_opens_an_empty_row() {
        assert_eq!(rows("one\ntwo", 12), vec!["one", "two"]);
        assert_eq!(rows("one\n", 12), vec!["one", ""]);
        assert_eq!(rows("", 12), vec![""]);
    }

    #[test]
    fn a_caret_at_a_wrapped_word_reads_from_the_row_the_word_moved_to() {
        let layout = ComposerLayout::new("hello wonderful world", 12);
        assert_eq!(
            layout.cursor_position(6),
            (1, 0),
            "the caret before a moved word sits where that word is drawn"
        );
        assert_eq!(layout.cursor_position(5), (0, 5), "the space stays behind");
        assert_eq!(layout.cursor_position(9), (1, 3));
        assert_eq!(layout.row_count(), 3);
    }

    #[test]
    fn a_caret_at_a_full_rows_right_edge_spills_to_the_row_below() {
        let filled = "x".repeat(12);
        let layout = ComposerLayout::new(&filled, 12);
        assert_eq!(layout.row_count(), 1, "a filled row is one row of text");
        assert_eq!(
            layout.cursor_position(12),
            (1, 0),
            "the next character typed goes to the row below"
        );
    }

    #[test]
    fn the_space_that_ends_a_row_stays_with_it_rather_than_indenting_the_next() {
        assert_eq!(
            rows("aaaaaaaaaaaa bbb", 12),
            vec!["aaaaaaaaaaaa ", "bbb"],
            "the moved word starts at the left edge"
        );
        assert_eq!(
            ComposerLayout::new("aaaa        ", 8).row_count(),
            1,
            "trailing spaces do not grow the composer by a row of blanks"
        );
    }

    #[test]
    fn a_caret_at_the_edge_of_a_line_a_newline_ended_waits_in_that_lines_margin() {
        let layout = ComposerLayout::new("aaaa\nbb", 4);
        assert_eq!(
            layout.cursor_position(4),
            (0, 4),
            "the end of a full line keeps its own row rather than the next line's"
        );
        assert_eq!(
            layout.cursor_position(5),
            (1, 0),
            "the next line starts fresh"
        );
    }

    #[test]
    fn wide_characters_are_measured_by_the_columns_they_occupy() {
        assert_eq!(rows("ab 🙂🙂", 5), vec!["ab ", "🙂🙂"]);
        let layout = ComposerLayout::new("🙂🙂", 3);
        assert_eq!(
            layout.cursor_position("🙂".len()),
            (1, 0),
            "a wide character that cannot finish a row starts the next one"
        );
    }
}
