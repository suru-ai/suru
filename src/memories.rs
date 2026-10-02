//! Memories: what a Sidekick chose to keep past its own Session, held by the
//! Server rather than by any Session or Provider, so a Sidekick on one
//! Provider recalls what a Sidekick on another stored (see `CONTEXT.md`).
//!
//! A Memory lives in the Server's own database, under the data root, so each
//! Channel keeps its own, and like everything there it outlives the Server's
//! stop (ADR 0006). It belongs to no Session: nothing ties it to the Session
//! that stored it, so deleting that Session leaves it standing, and it is
//! never read with a Session's history (ADR 0022) — only when a Sidekick asks
//! for Memories, and when a Sidekick's Provider is started, which is told the
//! titles of those most recently changed ([`MemoryIndex`]).
//!
//! This module decides what a Memory is and how much of one there may be.
//! Every bound is a count of characters, however many bytes each takes: a
//! title is one line of at most [`MAX_TITLE_CHARS`], since every Sidekick
//! begun later is shown titles before it recalls anything and they must stay
//! cheap; a body at most [`MAX_BODY_CHARS`], paid for only by a Sidekick that
//! recalls it; and at most [`MAX_TAGS`] tags of at most [`MAX_TAG_CHARS`]
//! each. A title is kept on one line and a tag in lower case without a
//! leading `#`, so a tag is matched however it is written. What a Sidekick is
//! refused for passing a bound is worded where its Tools are served.
//!
//! Search reads the full-text index the storage layer keeps over title, body
//! and tags — words, never embeddings — and answers rows carrying a
//! [`snippet`] of the body rather than the body itself. What an Agent writes
//! as a query is read into the index's own language in [`query`], so nothing
//! it writes is ever an error of that language's syntax.

mod query;

use std::fmt;

use serde::Serialize;

pub(crate) use query::{MAX_QUERY_WORDS, MatchQuery, QueryRefusal};

use crate::{
    protocol::SessionTimestamp,
    storage::{StorageError, StorageRepository},
};

/// The most characters a Memory's title holds.
pub(crate) const MAX_TITLE_CHARS: usize = 100;

/// The most characters a Memory's body holds.
pub(crate) const MAX_BODY_CHARS: usize = 10_000;

/// The most tags a Memory carries.
pub(crate) const MAX_TAGS: usize = 10;

/// The most characters one tag holds.
pub(crate) const MAX_TAG_CHARS: usize = 40;

/// How many titles a Sidekick's instructions name: those of the Memories most
/// recently changed. With every title within [`MAX_TITLE_CHARS`], the index
/// stays within a few thousand characters however many Memories there are.
pub(crate) const INDEXED_TITLES: usize = 30;

/// The most characters of a body a search's row carries, before the mark
/// saying it was cut.
pub(crate) const SNIPPET_CHARS: usize = 240;

/// What a Memory is known by: the number the Server gave it when it was
/// stored, never given to another Memory, a forgotten one's included, and the
/// same across every stop of the Server.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct MemoryId(i64);

impl MemoryId {
    pub(crate) const fn new(id: i64) -> Self {
        Self(id)
    }

    pub(crate) const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One Memory, whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Memory {
    pub(crate) id: MemoryId,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) tags: Vec<String>,
    pub(crate) stored_at: SessionTimestamp,
    pub(crate) changed_at: SessionTimestamp,
}

/// A Memory about to be stored, each part already within its bound and
/// normalised.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewMemory {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) tags: Vec<String>,
}

/// What a change of a Memory sets: each part it names, already within its
/// bound and normalised, the rest left as it is.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MemoryChange {
    pub(crate) title: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
}

impl MemoryChange {
    /// Whether the change names nothing to change.
    pub(crate) fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.tags.is_none()
    }
}

/// What a search asks for: the Memories its query finds, or every Memory
/// where it has none, narrowed to those carrying every one of its tags and
/// last changed at or after one moment and before another, and how many rows
/// at most.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemorySearch {
    pub(crate) query: Option<MatchQuery>,
    pub(crate) tags: Vec<String>,
    pub(crate) changed_after: Option<SessionTimestamp>,
    pub(crate) changed_before: Option<SessionTimestamp>,
    pub(crate) limit: usize,
}

/// One row of a search: a Memory without its body, and a snippet of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FoundMemory {
    pub(crate) id: MemoryId,
    pub(crate) title: String,
    pub(crate) tags: Vec<String>,
    pub(crate) stored_at: SessionTimestamp,
    pub(crate) changed_at: SessionTimestamp,
    pub(crate) snippet: String,
}

/// What a search found: its rows, best match first — or, with no query, most
/// recently changed first — and how many Memories matched in all, the rows
/// past its limit included.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FoundMemories {
    pub(crate) found: Vec<FoundMemory>,
    pub(crate) matched: usize,
}

/// What a Sidekick begins knowing of Memories: the titles of the
/// [`INDEXED_TITLES`] most recently changed, most recent first, with the
/// identity each is recalled by, and how many more are older — and nothing of
/// what any of them says. It is read as the Sidekick's Provider is started,
/// so it stands as Memories stood then; empty where there were none.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MemoryIndex {
    pub(crate) recent: Vec<IndexedMemory>,
    pub(crate) older: usize,
}

impl MemoryIndex {
    pub(crate) fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }
}

/// One title an index names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexedMemory {
    pub(crate) id: MemoryId,
    pub(crate) title: String,
}

/// `text` on one line: every run of whitespace or control characters, line
/// breaks among them, made one space, and none left at either end.
pub(crate) fn one_line(text: &str) -> String {
    text.split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A tag as a Memory carries it and a search matches it: on one line, in
/// lower case, and without a leading `#`. Empty where `written` holds nothing
/// but those.
pub(crate) fn tag(written: &str) -> String {
    one_line(written)
        .trim_start_matches('#')
        .trim_start()
        .to_lowercase()
}

/// The tags `written` names, as a Memory carries them: each as [`tag`] keeps
/// it, once, in the order first written, and none that holds nothing.
pub(crate) fn tags<'a>(written: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut kept = Vec::<String>::new();
    for tag in written.into_iter().map(tag) {
        if !tag.is_empty() && !kept.contains(&tag) {
            kept.push(tag);
        }
    }
    kept
}

/// A body as it is kept: as written, but for any NUL character, which the
/// index would read as its end.
pub(crate) fn body(written: &str) -> String {
    written.replace('\0', "")
}

/// What a search's row carries of `text`, a part of a body: on one line, and
/// cut short past [`SNIPPET_CHARS`] with a mark saying so.
pub(crate) fn snippet(text: &str) -> String {
    let line = one_line(text);
    if line.chars().count() <= SNIPPET_CHARS {
        return line;
    }
    let cut = line.chars().take(SNIPPET_CHARS).collect::<String>();
    format!("{}…", cut.trim_end())
}

/// The Memories this Server keeps, as a Sidekick's Tools and its
/// instructions reach them.
#[derive(Clone)]
pub(crate) struct MemoryStore {
    repository: StorageRepository,
}

impl MemoryStore {
    pub(crate) fn new(repository: StorageRepository) -> Self {
        Self { repository }
    }

    /// Stores `memory`, stored and changed now, and answers it as kept.
    pub(crate) async fn store(&self, memory: NewMemory) -> Result<Memory, StorageError> {
        self.repository.store_memory(memory).await
    }

    /// The Memory `id` names, whole, where one is kept.
    pub(crate) async fn recall(&self, id: MemoryId) -> Result<Option<Memory>, StorageError> {
        self.repository.memory(id).await
    }

    /// Changes the Memory `id` names as `change` says, changed now, and
    /// answers it as it stands, where one is kept.
    pub(crate) async fn change(
        &self,
        id: MemoryId,
        change: MemoryChange,
    ) -> Result<Option<Memory>, StorageError> {
        self.repository.change_memory(id, change).await
    }

    /// Forgets the Memory `id` names; answers whether one was kept to forget.
    pub(crate) async fn forget(&self, id: MemoryId) -> Result<bool, StorageError> {
        self.repository.forget_memory(id).await
    }

    /// The Memories `search` finds.
    pub(crate) async fn search(&self, search: MemorySearch) -> Result<FoundMemories, StorageError> {
        self.repository.search_memories(search).await
    }

    /// What a Sidekick started now begins knowing of Memories.
    pub(crate) async fn index(&self) -> Result<MemoryIndex, StorageError> {
        self.repository.memory_index(INDEXED_TITLES).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_kept_on_one_line() {
        assert_eq!(one_line("  Line one\n\tline two  "), "Line one line two");
        assert_eq!(one_line("a\u{0}b\u{7}c"), "a b c");
        assert_eq!(one_line(" \n "), "");
    }

    #[test]
    fn tags_are_kept_once_in_lower_case_without_a_leading_hash() {
        assert_eq!(
            tags([
                "#Rust",
                " rust ",
                "RUST",
                "",
                "Release   Notes",
                "##ci",
                "# Ünïcode"
            ]),
            ["rust", "release notes", "ci", "ünïcode"]
        );
        assert_eq!(tag("#"), "", "a tag of nothing but a hash holds nothing");
    }

    #[test]
    fn a_snippet_is_one_line_cut_short_past_its_bound() {
        assert_eq!(snippet("Short\nand sweet."), "Short and sweet.");
        let long = "é".repeat(SNIPPET_CHARS + 50);
        let cut = snippet(&long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            snippet(&"é".repeat(SNIPPET_CHARS)).chars().count(),
            SNIPPET_CHARS
        );
    }

    #[test]
    fn a_body_keeps_everything_but_nul() {
        assert_eq!(body("one\0two\nthree"), "onetwo\nthree");
    }
}
