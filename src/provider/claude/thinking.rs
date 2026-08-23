//! Splitting one Claude thinking block into Reasoning blocks, one per heading.
//!
//! Claude streams a thinking block as free prose. When the prose leads with a
//! bold heading the shared splitter titles the block with it, and every later
//! heading paragraph begins a new Reasoning block, so a block never carries
//! more than one title. Unheaded prose simply streams as the current block's
//! content. The scan withholds text only while a candidate heading is still
//! undecided — the same bargain the shared splitter strikes for a block's
//! leading title.

use super::super::reasoning::{ReasoningSegment, ReasoningSummarySplitter};

/// What separates the paragraphs of a thinking block, and so where a new
/// heading can begin.
const PARAGRAPH_SEPARATORS: [&str; 2] = ["\n\n", "\r\n\r\n"];

/// One resolution of the running split, in the order events must reach the
/// projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ThinkingEvent {
    /// The current Reasoning block resolved the title it leads with.
    Title(String),
    /// Content belonging to the current Reasoning block.
    Content(String),
    /// A heading paragraph: the current block is done and a new one begins,
    /// titled with the heading.
    Break { title: String },
}

/// The running split of one thinking block's stream.
pub(super) struct ThinkingSplitter {
    mode: Mode,
    /// Whether a resolving heading titles the block itself rather than
    /// breaking a new one — true only until the leading paragraph resolves.
    leading: bool,
    /// The separator withheld while the paragraph after it could still be a
    /// heading. Released as content when it is not, dropped when it is.
    pending_separator: String,
    /// Body characters withheld because they could still grow into a
    /// paragraph separator.
    tail: String,
    /// Decides whether the paragraph now streaming opens with a heading.
    decider: ReasoningSummarySplitter,
}

enum Mode {
    /// The current paragraph could still resolve into a heading, so its text
    /// is withheld in the decider.
    Deciding,
    /// The current paragraph is prose; content streams until a separator
    /// opens the next paragraph.
    Body,
}

impl Default for ThinkingSplitter {
    fn default() -> Self {
        Self {
            mode: Mode::Deciding,
            leading: true,
            pending_separator: String::new(),
            tail: String::new(),
            decider: ReasoningSummarySplitter::default(),
        }
    }
}

impl ThinkingSplitter {
    /// Admits the next chunk of thinking text, yielding whatever it resolved.
    pub(super) fn push(&mut self, delta: &str) -> Vec<ThinkingEvent> {
        let mut events = Vec::new();
        let mut input = delta.to_owned();
        loop {
            match self.mode {
                Mode::Deciding => {
                    let segment = self.decider.push(&input);
                    if !self.decider.is_settled() {
                        break;
                    }
                    input = self.resolve(segment, &mut events);
                }
                Mode::Body => {
                    let mut buffered = std::mem::take(&mut self.tail);
                    buffered.push_str(&input);
                    match find_separator(&buffered) {
                        Some((at, length)) => {
                            if at > 0 {
                                events.push(ThinkingEvent::Content(buffered[..at].to_owned()));
                            }
                            self.pending_separator = buffered[at..at + length].to_owned();
                            self.decider = ReasoningSummarySplitter::default();
                            self.mode = Mode::Deciding;
                            input = buffered[at + length..].to_owned();
                        }
                        None => {
                            let held = withheld_separator_prefix(&buffered);
                            let streamed = buffered.len() - held;
                            if streamed > 0 {
                                events
                                    .push(ThinkingEvent::Content(buffered[..streamed].to_owned()));
                            }
                            self.tail = buffered[streamed..].to_owned();
                            break;
                        }
                    }
                }
            }
        }
        events
    }

    /// Releases whatever the scan is still withholding, because the thinking
    /// block ended. A paragraph that is nothing but a heading still begins —
    /// or titles — its block; a trailing separator resolves to nothing.
    pub(super) fn finish(&mut self) -> Vec<ThinkingEvent> {
        let mut events = Vec::new();
        match self.mode {
            Mode::Deciding => {
                let segment = self.decider.finish();
                let ends_empty = segment.title.is_none() && segment.content.is_empty();
                if !ends_empty {
                    let content = self.resolve(segment, &mut events);
                    if !content.is_empty() {
                        events.push(ThinkingEvent::Content(content));
                    }
                }
            }
            Mode::Body => {
                if !self.tail.is_empty() {
                    events.push(ThinkingEvent::Content(std::mem::take(&mut self.tail)));
                }
            }
        }
        events
    }

    /// Applies a decided paragraph: a heading titles or breaks a block and
    /// swallows the separator before it; prose returns the separator to the
    /// current block. Yields the paragraph's remaining text for the body scan.
    fn resolve(&mut self, segment: ReasoningSegment, events: &mut Vec<ThinkingEvent>) -> String {
        self.mode = Mode::Body;
        match segment.title {
            Some(title) => {
                self.pending_separator.clear();
                events.push(if self.leading {
                    ThinkingEvent::Title(title)
                } else {
                    ThinkingEvent::Break { title }
                });
            }
            None => {
                if !self.pending_separator.is_empty() {
                    events.push(ThinkingEvent::Content(std::mem::take(
                        &mut self.pending_separator,
                    )));
                }
            }
        }
        self.leading = false;
        segment.content
    }
}

/// Where the earliest paragraph separator sits in `text`, and how long it is.
fn find_separator(text: &str) -> Option<(usize, usize)> {
    PARAGRAPH_SEPARATORS
        .iter()
        .filter_map(|separator| text.find(separator).map(|at| (at, separator.len())))
        .min()
}

/// How many trailing bytes of `text` are a partial paragraph separator, and so
/// must be withheld until more text says whether one is forming.
fn withheld_separator_prefix(text: &str) -> usize {
    ["\r\n\r", "\r\n", "\n", "\r"]
        .into_iter()
        .find(|partial| text.ends_with(partial))
        .map_or(0, str::len)
}

#[cfg(test)]
mod tests {
    use super::{ThinkingEvent, ThinkingSplitter};

    fn title(text: &str) -> ThinkingEvent {
        ThinkingEvent::Title(text.to_owned())
    }

    fn content(text: &str) -> ThinkingEvent {
        ThinkingEvent::Content(text.to_owned())
    }

    fn heading_break(text: &str) -> ThinkingEvent {
        ThinkingEvent::Break {
            title: text.to_owned(),
        }
    }

    #[test]
    fn unheaded_thinking_streams_straight_through_as_content() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Reading the"), vec![content("Reading the")]);
        assert_eq!(splitter.push(" projection."), vec![content(" projection.")]);
        assert_eq!(splitter.finish(), vec![]);
    }

    #[test]
    fn a_leading_heading_titles_the_block() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(
            splitter.push("**Plan**\n\nRead the seam first."),
            vec![title("Plan"), content("Read the seam first.")]
        );
    }

    #[test]
    fn a_heading_after_prose_begins_a_new_block() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(
            splitter.push("Looking around.\n\n**Next**\n\nActing on it."),
            vec![
                content("Looking around."),
                heading_break("Next"),
                content("Acting on it."),
            ]
        );
    }

    #[test]
    fn every_heading_in_one_delta_begins_its_own_block() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(
            splitter.push("**A**\n\nfirst\n\n**B**\n\nsecond"),
            vec![
                title("A"),
                content("first"),
                heading_break("B"),
                content("second"),
            ]
        );
    }

    #[test]
    fn a_paragraph_that_merely_opens_bold_stays_in_the_current_block() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(
            splitter.push("Prose.\n\n**Careful**: the seam is shared."),
            vec![
                content("Prose."),
                content("\n\n"),
                content("**Careful**: the seam is shared."),
            ]
        );
    }

    #[test]
    fn a_candidate_heading_is_withheld_until_it_resolves() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Prose.\n\n**He"), vec![content("Prose.")]);
        assert_eq!(splitter.push("ad**"), vec![]);
        assert_eq!(
            splitter.push("\n\nBody."),
            vec![heading_break("Head"), content("Body.")]
        );
    }

    #[test]
    fn a_separator_split_across_deltas_is_still_a_paragraph_break() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Prose.\n"), vec![content("Prose.")]);
        assert_eq!(
            splitter.push("\n**Next**\n\nMore."),
            vec![heading_break("Next"), content("More."),]
        );
    }

    #[test]
    fn finishing_releases_a_candidate_that_never_resolved_as_content() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Prose.\n\n**Tail"), vec![content("Prose.")]);
        assert_eq!(splitter.finish(), vec![content("\n\n"), content("**Tail")]);
    }

    #[test]
    fn a_block_that_ends_on_a_heading_still_begins_its_block() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Prose.\n\n**Done**"), vec![content("Prose.")]);
        assert_eq!(splitter.finish(), vec![heading_break("Done")]);
    }

    #[test]
    fn a_trailing_separator_resolves_to_nothing() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(splitter.push("Prose.\n\n"), vec![content("Prose.")]);
        assert_eq!(splitter.finish(), vec![]);
    }

    #[test]
    fn crlf_separators_break_paragraphs_like_bare_newlines() {
        let mut splitter = ThinkingSplitter::default();

        assert_eq!(
            splitter.push("**T**\r\n\r\nBody.\r\n\r\n**U**\r\n\r\nMore."),
            vec![
                title("T"),
                content("Body."),
                heading_break("U"),
                content("More."),
            ]
        );
    }
}
