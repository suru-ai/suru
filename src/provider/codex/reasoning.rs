//! Splitting the title Codex leads a Reasoning summary with off its body.
//!
//! OpenAI's Responses API opens each reasoning summary section with a bolded
//! title block — `**Inspecting the provider seam**`, a blank line, then the
//! prose. Suru stores that title as a typed property of the Reasoning Activity
//! the section becomes, rather than leaving it in the content, so a client can
//! head a folded block with it instead of parsing it back out. The split happens
//! as the section streams, which is why it needs the state this module holds:
//! the head is withheld until the block either resolves into a title or is
//! ruled out.

/// The most characters withheld while the title block is undecided. A title is
/// a short phrase, so a head that runs past this is prose that merely happened
/// to open with an asterisk, and holding it back any longer only delays it.
const MAX_TITLE_BLOCK_CHARS: usize = 256;

/// What separates a title block from the body beneath it.
const TITLE_SEPARATORS: [&str; 2] = ["\n\n", "\r\n\r\n"];

/// One step of a Reasoning summary, split into the parts Suru stores apart from
/// one another. A step that resolved no title and withheld its content yields
/// both empty, which the caller projects as no Provider event at all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReasoningSegment {
    pub(super) title: Option<String>,
    pub(super) content: String,
}

/// The running split of one Reasoning summary section's stream.
///
/// A summary can run to several sections, each opening with its own bold
/// heading, and each becoming a Reasoning Activity of its own. The projection
/// therefore starts a splitter per section, so every heading titles the block
/// its own section fills rather than being buried in the body of the block
/// before it, and no block ever carries more than one title.
#[derive(Debug, Default)]
pub(super) struct ReasoningSummarySplitter {
    /// The head withheld while the title block is undecided.
    head: String,
    /// Whether the title block has been decided, after which every delta is
    /// body and passes straight through.
    settled: bool,
}

impl ReasoningSummarySplitter {
    /// Admits the next chunk of summary text, yielding whatever it resolved.
    pub(super) fn push(&mut self, delta: &str) -> ReasoningSegment {
        if self.settled {
            return ReasoningSegment {
                title: None,
                content: delta.to_owned(),
            };
        }
        self.head.push_str(delta);
        match scan_title(&self.head, false) {
            TitleScan::Undecided => ReasoningSegment::default(),
            TitleScan::Titled { title, body } => {
                self.settled = true;
                self.head = String::new();
                ReasoningSegment {
                    title: Some(title),
                    content: body,
                }
            }
            TitleScan::Untitled => {
                self.settled = true;
                ReasoningSegment {
                    title: None,
                    content: std::mem::take(&mut self.head),
                }
            }
        }
    }

    /// Releases whatever the splitter is still withholding, because the summary
    /// ended before the title block could resolve on its own. A summary that is
    /// nothing but a title block is still titled; anything else is body.
    pub(super) fn finish(&mut self) -> ReasoningSegment {
        if self.settled {
            return ReasoningSegment::default();
        }
        self.settled = true;
        let head = std::mem::take(&mut self.head);
        match scan_title(&head, true) {
            TitleScan::Titled { title, body } => ReasoningSegment {
                title: Some(title),
                content: body,
            },
            TitleScan::Undecided | TitleScan::Untitled => ReasoningSegment {
                title: None,
                content: head,
            },
        }
    }
}

enum TitleScan {
    /// The head could still become a title block once more of it arrives.
    Undecided,
    Titled {
        title: String,
        body: String,
    },
    Untitled,
}

/// Decides what the head of a Reasoning summary is. `ending` reports that no
/// more text is coming, which resolves a complete title block still waiting on
/// the blank line that would have separated it from a body.
fn scan_title(head: &str, ending: bool) -> TitleScan {
    let undecided = if ending {
        TitleScan::Untitled
    } else {
        TitleScan::Undecided
    };
    if head.chars().count() > MAX_TITLE_BLOCK_CHARS {
        return TitleScan::Untitled;
    }
    let opened = head.trim_start();
    let Some(after) = opened.strip_prefix("**") else {
        // A lone leading asterisk is the start of either a title block or
        // ordinary emphasis, and only the next character says which.
        return if opened.is_empty() || opened == "*" {
            undecided
        } else {
            TitleScan::Untitled
        };
    };
    let Some(end) = after.find("**") else {
        return if after.contains('\n') {
            TitleScan::Untitled
        } else {
            undecided
        };
    };
    let title = &after[..end];
    if title.trim().is_empty() || title.contains(['\n', '*']) {
        return TitleScan::Untitled;
    }
    let rest = &after[end + 2..];
    if let Some(body) = TITLE_SEPARATORS
        .iter()
        .find_map(|separator| rest.strip_prefix(separator))
    {
        return TitleScan::Titled {
            title: title.trim().to_owned(),
            body: body.to_owned(),
        };
    }
    if TITLE_SEPARATORS
        .iter()
        .any(|separator| separator.starts_with(rest))
    {
        // The block is complete but its separator is not, so only the end of
        // the summary can tell a title from bold text that opens a sentence.
        return if ending {
            TitleScan::Titled {
                title: title.trim().to_owned(),
                body: String::new(),
            }
        } else {
            TitleScan::Undecided
        };
    }
    TitleScan::Untitled
}

#[cfg(test)]
mod tests {
    use super::{MAX_TITLE_BLOCK_CHARS, ReasoningSegment, ReasoningSummarySplitter};

    fn titled(title: &str, content: &str) -> ReasoningSegment {
        ReasoningSegment {
            title: Some(title.to_owned()),
            content: content.to_owned(),
        }
    }

    fn body(content: &str) -> ReasoningSegment {
        ReasoningSegment {
            title: None,
            content: content.to_owned(),
        }
    }

    #[test]
    fn a_leading_bold_block_becomes_the_title_and_leaves_the_body_behind() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(
            splitter.push("**Inspecting the seam**\n\nReading the projection."),
            titled("Inspecting the seam", "Reading the projection.")
        );
    }

    #[test]
    fn a_title_split_across_deltas_is_withheld_until_it_resolves() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(splitter.push("**Inspec"), ReasoningSegment::default());
        assert_eq!(splitter.push("ting the seam"), ReasoningSegment::default());
        assert_eq!(splitter.push("**"), ReasoningSegment::default());
        assert_eq!(splitter.push("\n"), ReasoningSegment::default());
        assert_eq!(
            splitter.push("\nReading"),
            titled("Inspecting the seam", "Reading")
        );
        assert_eq!(splitter.push(" the projection."), body(" the projection."));
    }

    #[test]
    fn a_summary_that_is_only_a_title_keeps_it_as_the_title() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(
            splitter.push("**Inspecting the seam**"),
            ReasoningSegment::default()
        );
        assert_eq!(splitter.finish(), titled("Inspecting the seam", ""));
    }

    #[test]
    fn a_summary_that_does_not_open_with_a_bold_block_is_all_body() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(splitter.push("Reading the"), body("Reading the"));
        assert_eq!(splitter.push(" projection."), body(" projection."));
    }

    #[test]
    fn bold_text_that_opens_a_sentence_is_body_rather_than_a_title() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(
            splitter.push("**Careful**: the seam is shared.\n\nSo read it first."),
            body("**Careful**: the seam is shared.\n\nSo read it first.")
        );
    }

    #[test]
    fn a_bold_run_that_spans_lines_is_body_rather_than_a_title() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(
            splitter.push("**Careful\nnow**\n\nReading."),
            body("**Careful\nnow**\n\nReading.")
        );
    }

    #[test]
    fn a_head_that_outruns_the_title_budget_is_released_as_body() {
        let mut splitter = ReasoningSummarySplitter::default();
        let overlong = format!("**{}", "a".repeat(MAX_TITLE_BLOCK_CHARS));

        assert_eq!(splitter.push(&overlong), body(&overlong));
    }

    #[test]
    fn finishing_a_settled_summary_releases_nothing_further() {
        let mut splitter = ReasoningSummarySplitter::default();

        assert_eq!(
            splitter.push("**Seam**\n\nReading."),
            titled("Seam", "Reading.")
        );
        assert_eq!(splitter.finish(), ReasoningSegment::default());
    }
}
