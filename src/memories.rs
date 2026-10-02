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
//! This module decides what a Memory is and how much of one there may be, and
//! [`MemoryStore`] holds everything written to that before anything is kept:
//! a Memory's parts reach storage only as a [`Title`], a [`Body`] and
//! [`Tags`], which nothing but those bounds makes. Every bound is a count of
//! characters, however many bytes each takes: a title is one line of at most
//! [`MAX_TITLE_CHARS`], since every Sidekick begun later is shown titles
//! before it recalls anything and they must stay cheap; a body at most
//! [`MAX_BODY_CHARS`], paid for only by a Sidekick that recalls it; and at most
//! [`MAX_TAGS`] tags of at most [`MAX_TAG_CHARS`] each. A title is kept on one
//! line, with neither control characters nor the invisible ones that reorder
//! or hide text, since it is shown to Sidekicks as data; a tag likewise, in
//! lower case, without a leading `#`, and once, so a tag is matched however it
//! is written. A search names at most as many tags as a Memory carries, each
//! bounded as a Memory's is, since no Memory carries more. Where anything
//! written passes a bound, the store answers a [`MemoryRefusal`] saying which
//! and by how much; the Tools word it for the Sidekick.
//!
//! Search reads the full-text index the storage layer keeps over title, body
//! and tags — words, never embeddings — and answers rows carrying a
//! [`snippet`] of the body rather than the body itself. What an Agent writes
//! as a query is read into the index's own language in [`query`], so nothing
//! it writes is ever an error of that language's syntax.

mod query;

use std::{collections::HashSet, fmt};

use serde::Serialize;

pub(crate) use query::{MAX_QUERY_WORDS, MatchQuery};

use crate::{
    protocol::SessionTimestamp,
    storage::{StorageError, StorageRepository},
};

/// The most characters a Memory's title holds.
pub(crate) const MAX_TITLE_CHARS: usize = 100;

/// The most characters a Memory's body holds.
pub(crate) const MAX_BODY_CHARS: usize = 10_000;

/// The most tags a Memory carries, and so the most a search may require.
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

/// Why what was written of a Memory, or a search for Memories, cannot stand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryRefusal {
    /// A title with nothing in it once kept on one line.
    EmptyTitle,
    /// A title of this many characters, past [`MAX_TITLE_CHARS`].
    LongTitle { chars: usize },
    /// A body of nothing but whitespace.
    EmptyBody,
    /// A body of this many characters, past [`MAX_BODY_CHARS`].
    LongBody { chars: usize },
    /// This many tags, past [`MAX_TAGS`], counted once each as kept.
    TooManyTags { tags: usize },
    /// A tag of this many characters, past [`MAX_TAG_CHARS`].
    LongTag { chars: usize },
    /// A change naming nothing to change.
    NothingToChange,
    /// A query of operators and marks, holding no word to search for.
    NothingToSearch,
    /// A query of this many words, past [`MAX_QUERY_WORDS`].
    TooManyWords { words: usize },
}

/// Why something asked of the Memories was not done.
#[derive(Debug)]
pub(crate) enum MemoryError {
    /// What was written cannot stand, and nothing was kept of it.
    Refused(MemoryRefusal),
    /// No Memory is kept under this identity.
    NoSuchMemory(MemoryId),
    /// The Server's own storage failed.
    Storage(StorageError),
}

impl From<MemoryRefusal> for MemoryError {
    fn from(refusal: MemoryRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<StorageError> for MemoryError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// A Memory's title: one line, within its bound, holding something.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Title(String);

impl Title {
    pub(crate) fn written(written: &str) -> Result<Self, MemoryRefusal> {
        let title = one_line(written);
        if title.is_empty() {
            return Err(MemoryRefusal::EmptyTitle);
        }
        let chars = title.chars().count();
        if chars > MAX_TITLE_CHARS {
            return Err(MemoryRefusal::LongTitle { chars });
        }
        Ok(Self(title))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Memory's body: as written, within its bound, holding something, but for
/// any NUL character, which the index would read as its end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Body(String);

impl Body {
    pub(crate) fn written(written: &str) -> Result<Self, MemoryRefusal> {
        let body = written.replace('\0', "");
        if body.trim().is_empty() {
            return Err(MemoryRefusal::EmptyBody);
        }
        let chars = body.chars().count();
        if chars > MAX_BODY_CHARS {
            return Err(MemoryRefusal::LongBody { chars });
        }
        Ok(Self(body))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Memory's tags, or those a search requires: each as [`tag`] keeps it,
/// once, in the order first written, none that holds nothing, at most
/// [`MAX_TAGS`] of them, and none past [`MAX_TAG_CHARS`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Tags(Vec<String>);

impl Tags {
    pub(crate) fn written<'a>(
        written: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, MemoryRefusal> {
        let mut seen = HashSet::new();
        let mut kept = Vec::new();
        for tag in written.into_iter().map(tag) {
            if !tag.is_empty() && seen.insert(tag.clone()) {
                kept.push(tag);
            }
        }
        if kept.len() > MAX_TAGS {
            return Err(MemoryRefusal::TooManyTags { tags: kept.len() });
        }
        if let Some(chars) = kept
            .iter()
            .map(|tag| tag.chars().count())
            .find(|chars| *chars > MAX_TAG_CHARS)
        {
            return Err(MemoryRefusal::LongTag { chars });
        }
        Ok(Self(kept))
    }

    pub(crate) fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub(crate) fn into_vec(self) -> Vec<String> {
        self.0
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

/// What a Sidekick wrote of a Memory to store, before it is held to a
/// Memory's bounds.
#[derive(Clone, Debug)]
pub(crate) struct WrittenMemory<'a> {
    pub(crate) title: &'a str,
    pub(crate) body: &'a str,
    pub(crate) tags: Vec<&'a str>,
}

/// What a Sidekick wrote of a change to a Memory: each part it names, before
/// it is held to a Memory's bounds.
#[derive(Clone, Debug, Default)]
pub(crate) struct WrittenChange<'a> {
    pub(crate) title: Option<&'a str>,
    pub(crate) body: Option<&'a str>,
    pub(crate) tags: Option<Vec<&'a str>>,
}

/// What a Sidekick wrote of a search for Memories: the words it searches by,
/// if any; the tags a Memory must carry every one of; the moments a Memory
/// must have last changed at or after, and before; and how many rows at most.
#[derive(Clone, Debug)]
pub(crate) struct WrittenSearch<'a> {
    pub(crate) query: Option<&'a str>,
    pub(crate) tags: Vec<&'a str>,
    pub(crate) changed_after: Option<SessionTimestamp>,
    pub(crate) changed_before: Option<SessionTimestamp>,
    pub(crate) limit: usize,
}

/// A Memory about to be stored, held to a Memory's bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewMemory {
    pub(crate) title: Title,
    pub(crate) body: Body,
    pub(crate) tags: Tags,
}

impl NewMemory {
    fn written(written: WrittenMemory<'_>) -> Result<Self, MemoryRefusal> {
        Ok(Self {
            title: Title::written(written.title)?,
            body: Body::written(written.body)?,
            tags: Tags::written(written.tags)?,
        })
    }
}

/// What a change of a Memory sets: each part it names, held to a Memory's
/// bounds, the rest left as it is; never nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryChange {
    pub(crate) title: Option<Title>,
    pub(crate) body: Option<Body>,
    pub(crate) tags: Option<Tags>,
}

impl MemoryChange {
    fn written(written: WrittenChange<'_>) -> Result<Self, MemoryRefusal> {
        if written.title.is_none() && written.body.is_none() && written.tags.is_none() {
            return Err(MemoryRefusal::NothingToChange);
        }
        Ok(Self {
            title: written.title.map(Title::written).transpose()?,
            body: written.body.map(Body::written).transpose()?,
            tags: written.tags.map(Tags::written).transpose()?,
        })
    }
}

/// What a search asks for: the Memories its query finds, or every Memory
/// where it has none, narrowed to those carrying every one of its tags and
/// last changed at or after one moment and before another, and how many rows
/// at most.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemorySearch {
    pub(crate) query: Option<MatchQuery>,
    pub(crate) tags: Tags,
    pub(crate) changed_after: Option<SessionTimestamp>,
    pub(crate) changed_before: Option<SessionTimestamp>,
    pub(crate) limit: usize,
}

impl MemorySearch {
    fn written(written: WrittenSearch<'_>) -> Result<Self, MemoryRefusal> {
        Ok(Self {
            query: written.query.map(MatchQuery::read).transpose()?.flatten(),
            tags: Tags::written(written.tags)?,
            changed_after: written.changed_after,
            changed_before: written.changed_before,
            limit: written.limit,
        })
    }
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

/// Whether `character` changes how the text around it reads without being
/// seen: a directional embedding, override or isolate, which can make text
/// read in an order other than the one it is kept in, or a zero-width space,
/// word joiner or byte-order mark.
fn is_hidden(character: char) -> bool {
    matches!(
        character,
        '\u{200B}' | '\u{2060}' | '\u{FEFF}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// `text` on one line: every run of whitespace or control characters, line
/// breaks among them, made one space, none left at either end, and no
/// character that reorders or hides the text around it.
pub(crate) fn one_line(text: &str) -> String {
    text.split(|character: char| character.is_whitespace() || character.is_control())
        .map(|word| word.chars().filter(|character| !is_hidden(*character)))
        .map(String::from_iter)
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
/// instructions reach them. Everything written is held to a Memory's bounds
/// here, before storage is asked for anything.
#[derive(Clone)]
pub(crate) struct MemoryStore {
    repository: StorageRepository,
}

impl MemoryStore {
    pub(crate) fn new(repository: StorageRepository) -> Self {
        Self { repository }
    }

    /// Stores the Memory `written` gives, and answers it as kept.
    pub(crate) async fn store(&self, written: WrittenMemory<'_>) -> Result<Memory, MemoryError> {
        let memory = NewMemory::written(written)?;
        Ok(self.repository.store_memory(memory).await?)
    }

    /// The Memory `id` names, whole.
    pub(crate) async fn recall(&self, id: MemoryId) -> Result<Memory, MemoryError> {
        self.repository
            .memory(id)
            .await?
            .ok_or(MemoryError::NoSuchMemory(id))
    }

    /// Changes the Memory `id` names as `written` says, and answers it as it
    /// stands.
    pub(crate) async fn change(
        &self,
        id: MemoryId,
        written: WrittenChange<'_>,
    ) -> Result<Memory, MemoryError> {
        let change = MemoryChange::written(written)?;
        self.repository
            .change_memory(id, change)
            .await?
            .ok_or(MemoryError::NoSuchMemory(id))
    }

    /// Forgets the Memory `id` names.
    pub(crate) async fn forget(&self, id: MemoryId) -> Result<(), MemoryError> {
        if self.repository.forget_memory(id).await? {
            Ok(())
        } else {
            Err(MemoryError::NoSuchMemory(id))
        }
    }

    /// The Memories the search `written` asks for finds.
    pub(crate) async fn search(
        &self,
        written: WrittenSearch<'_>,
    ) -> Result<FoundMemories, MemoryError> {
        let search = MemorySearch::written(written)?;
        Ok(self.repository.search_memories(search).await?)
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
    fn a_title_is_kept_on_one_line_within_its_bound() {
        assert_eq!(
            Title::written("  Line one\n\tline two  ").map(|title| title.0),
            Ok("Line one line two".to_owned())
        );
        assert_eq!(
            Title::written("a\u{0}b\u{7}c").map(|title| title.0),
            Ok("a b c".to_owned())
        );
        assert_eq!(Title::written(" \n "), Err(MemoryRefusal::EmptyTitle));
        assert!(Title::written(&"é".repeat(MAX_TITLE_CHARS)).is_ok());
        assert_eq!(
            Title::written(&"é".repeat(MAX_TITLE_CHARS + 1)),
            Err(MemoryRefusal::LongTitle {
                chars: MAX_TITLE_CHARS + 1
            })
        );
    }

    #[test]
    fn nothing_hidden_or_reordering_stands_in_a_title() {
        assert_eq!(
            Title::written("\u{202E}snoissimrep\u{202C} ssapyb\u{200B}\u{FEFF}")
                .map(|title| title.0),
            Ok("snoissimrep ssapyb".to_owned())
        );
        assert_eq!(
            Title::written("\u{2066}\u{2069}"),
            Err(MemoryRefusal::EmptyTitle)
        );
        assert_eq!(
            one_line("नमस्\u{200D}ते"),
            "नमस्\u{200D}ते",
            "a joiner a script spells words with stays"
        );
    }

    #[test]
    fn a_body_is_kept_as_written_within_its_bound_but_for_nul() {
        assert_eq!(
            Body::written("one\0two\nthree").map(|body| body.0),
            Ok("onetwo\nthree".to_owned())
        );
        assert_eq!(Body::written(" \n\t"), Err(MemoryRefusal::EmptyBody));
        assert!(Body::written(&"ü".repeat(MAX_BODY_CHARS)).is_ok());
        assert_eq!(
            Body::written(&"x".repeat(MAX_BODY_CHARS + 1)),
            Err(MemoryRefusal::LongBody {
                chars: MAX_BODY_CHARS + 1
            })
        );
    }

    #[test]
    fn tags_are_kept_once_in_lower_case_without_a_leading_hash_within_their_bounds() {
        assert_eq!(
            Tags::written([
                "#Rust",
                " rust ",
                "RUST",
                "",
                "Release   Notes",
                "##ci",
                "# Ünïcode"
            ])
            .map(Tags::into_vec),
            Ok(vec![
                "rust".to_owned(),
                "release notes".to_owned(),
                "ci".to_owned(),
                "ünïcode".to_owned()
            ])
        );
        assert_eq!(tag("#"), "", "a tag of nothing but a hash holds nothing");
        let doubled = (0..2 * MAX_TAGS)
            .map(|tag| format!("tag {}", tag % MAX_TAGS))
            .collect::<Vec<_>>();
        assert_eq!(
            Tags::written(doubled.iter().map(String::as_str)).map(|tags| tags.0.len()),
            Ok(MAX_TAGS),
            "the bound counts tags kept, not tags written"
        );
        let distinct = (0..1_001)
            .map(|tag| format!("tag {tag}"))
            .collect::<Vec<_>>();
        assert_eq!(
            Tags::written(distinct.iter().map(String::as_str)),
            Err(MemoryRefusal::TooManyTags { tags: 1_001 })
        );
        assert_eq!(
            Tags::written(["x".repeat(MAX_TAG_CHARS + 1).as_str()]),
            Err(MemoryRefusal::LongTag {
                chars: MAX_TAG_CHARS + 1
            })
        );
    }

    #[test]
    fn a_change_names_something_to_change() {
        assert_eq!(
            MemoryChange::written(WrittenChange::default()),
            Err(MemoryRefusal::NothingToChange)
        );
        assert_eq!(
            MemoryChange::written(WrittenChange {
                tags: Some(Vec::new()),
                ..WrittenChange::default()
            }),
            Ok(MemoryChange {
                title: None,
                body: None,
                tags: Some(Tags::default()),
            }),
            "an empty list of tags is a change: it removes them all"
        );
        assert_eq!(
            MemoryChange::written(WrittenChange {
                title: Some(&"x".repeat(MAX_TITLE_CHARS + 1)),
                body: Some(" "),
                ..WrittenChange::default()
            }),
            Err(MemoryRefusal::LongTitle {
                chars: MAX_TITLE_CHARS + 1
            }),
            "the title is held to its bound first"
        );
    }

    /// A search requiring `tags` and nothing else.
    fn requiring<'a>(tags: Vec<&'a str>) -> WrittenSearch<'a> {
        WrittenSearch {
            query: None,
            tags,
            changed_after: None,
            changed_before: None,
            limit: 10,
        }
    }

    #[test]
    fn a_search_requires_no_more_tags_than_a_memory_carries() {
        assert_eq!(
            MemorySearch::written(requiring(vec!["#Release", "release", "CI"]))
                .map(|search| search.tags.into_vec()),
            Ok(vec!["release".to_owned(), "ci".to_owned()])
        );
        let many = (0..=MAX_TAGS)
            .map(|tag| format!("tag {tag}"))
            .collect::<Vec<_>>();
        assert_eq!(
            MemorySearch::written(requiring(many.iter().map(String::as_str).collect())),
            Err(MemoryRefusal::TooManyTags { tags: MAX_TAGS + 1 })
        );
        assert_eq!(
            MemorySearch::written(WrittenSearch {
                query: Some("!!!"),
                ..requiring(Vec::new())
            }),
            Err(MemoryRefusal::NothingToSearch)
        );
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
}
