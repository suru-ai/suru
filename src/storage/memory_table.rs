//! Memory rows, and the full-text index a search reads over them.
//!
//! Each Memory is one row of `memories`, its tags a JSON array of strings.
//! The index, `memories_fts`, is FTS5's, built into the bundled SQLite on
//! every platform; it holds no copy of the text and is kept in step with
//! `memories` by triggers the migration made, so every write here touches
//! `memories` alone and none can leave the index behind. Nothing here reads a
//! Session: Memories are no Session's, and are read only when asked for.
//!
//! Every write runs in an immediate transaction, taking the database's write
//! lock before it reads, so two Sidekicks storing or changing Memories at once
//! each land whole, one after the other, and a change never builds on a row
//! another change has since replaced. A write reads the moment it lands at
//! while it holds that lock, and never stamps it earlier than a millisecond
//! past the latest moment any Memory was last changed, so the moments run in
//! the order writes land, whatever the clock does or however long a write
//! waited for the lock: the Memories most recently changed, and a range of
//! moments, mean the order things happened in.

use diesel::{
    QueryableByName, SqliteConnection,
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{CountRow, StorageError, StorageRepository, memories, on_blocking_task};
use crate::{
    clock::ServerClock,
    memories::{
        FoundMemories, FoundMemory, IndexedMemory, Memory, MemoryChange, MemoryId, MemoryIndex,
        MemorySearch, NewMemory, SNIPPET_CHARS, snippet,
    },
    protocol::SessionTimestamp,
};

/// How many of the index's tokens a snippet spans at most, around the words
/// that matched; [`snippet`] bounds its characters besides, since one token
/// may be long.
const SNIPPET_TOKENS: usize = 24;

/// How much a match weighs in each column, in the index's column order:
/// title, body, tags. A word in a title or a tag says more of what a Memory
/// is about than the same word in its body.
const COLUMN_WEIGHTS: &str = "10.0, 1.0, 5.0";

#[derive(Queryable, Selectable)]
#[diesel(table_name = memories)]
struct MemoryRow {
    id: i64,
    title: String,
    body: String,
    tags: String,
    stored_at: i64,
    changed_at: i64,
}

#[derive(Insertable)]
#[diesel(table_name = memories)]
struct NewMemoryRow<'a> {
    title: &'a str,
    body: &'a str,
    tags: &'a str,
    stored_at: i64,
    changed_at: i64,
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        Memory {
            id: MemoryId::new(self.id),
            tags: stored_tags(self.id, &self.tags),
            title: self.title,
            body: self.body,
            stored_at: moment(self.stored_at),
            changed_at: moment(self.changed_at),
        }
    }
}

/// One row a search answers, read with its snippet rather than its body.
#[derive(QueryableByName)]
struct FoundRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    tags: String,
    #[diesel(sql_type = BigInt)]
    stored_at: i64,
    #[diesel(sql_type = BigInt)]
    changed_at: i64,
    #[diesel(sql_type = Text)]
    snippet: String,
}

/// A value a search binds to one of its placeholders.
enum Bound {
    Text(String),
    Moment(i64),
}

/// A moment as the `stored_at` and `changed_at` columns store it.
fn millis(moment: SessionTimestamp) -> i64 {
    i64::try_from(moment.0).unwrap_or(i64::MAX)
}

/// A moment the `stored_at` and `changed_at` columns stored.
fn moment(millis: i64) -> SessionTimestamp {
    SessionTimestamp(u64::try_from(millis).unwrap_or(0))
}

/// Tags as the `tags` column stores them.
fn tags_column(tags: &[String]) -> String {
    serde_json::to_string(tags).expect("a list of strings always serializes")
}

/// The tags the `tags` column of Memory `id` stored. Ones that no longer
/// decode are read as none, and said so to the Log, rather than keeping the
/// Memory from being read at all.
fn stored_tags(id: i64, column: &str) -> Vec<String> {
    serde_json::from_str(column).unwrap_or_else(|error| {
        tracing::warn!(memory_id = id, "a Memory's tags are unreadable: {error}");
        Vec::new()
    })
}

fn read_error(error: diesel::result::Error) -> StorageError {
    StorageError::Read(error.to_string())
}

fn write_error(error: diesel::result::Error) -> StorageError {
    StorageError::WriteMemory(error.to_string())
}

/// The moment a write landing now is stamped with, read while it holds the
/// write lock: the clock's, or a millisecond past the latest moment any
/// Memory was last changed where the clock reads no later.
fn landing_moment(
    connection: &mut SqliteConnection,
    clock: &ServerClock,
) -> Result<i64, diesel::result::Error> {
    let latest = memories::table
        .select(diesel::dsl::max(memories::changed_at))
        .first::<Option<i64>>(connection)?;
    let now = millis(clock.now());
    Ok(latest.map_or(now, |latest| now.max(latest.saturating_add(1))))
}

fn kept_memory(
    connection: &mut SqliteConnection,
    id: MemoryId,
) -> Result<Option<Memory>, diesel::result::Error> {
    memories::table
        .find(id.get())
        .select(MemoryRow::as_select())
        .first(connection)
        .optional()
        .map(|row| row.map(MemoryRow::into_memory))
}

impl StorageRepository {
    /// Stores `memory`, stored and changed as it lands, and answers it as
    /// kept.
    pub(crate) async fn store_memory(&self, memory: NewMemory) -> Result<Memory, StorageError> {
        let path = self.database_path.clone();
        let clock = self.clock.clone();
        on_blocking_task("store Memory", move || {
            let mut connection = super::connect(&path)?;
            let tags = tags_column(memory.tags.as_slice());
            connection
                .immediate_transaction(|connection| {
                    let now = landing_moment(connection, &clock)?;
                    diesel::insert_into(memories::table)
                        .values(NewMemoryRow {
                            title: memory.title.as_str(),
                            body: memory.body.as_str(),
                            tags: &tags,
                            stored_at: now,
                            changed_at: now,
                        })
                        .execute(connection)?;
                    let id = diesel::select(diesel::dsl::sql::<BigInt>("last_insert_rowid()"))
                        .get_result::<i64>(connection)?;
                    Ok(Memory {
                        id: MemoryId::new(id),
                        title: memory.title.as_str().to_owned(),
                        body: memory.body.as_str().to_owned(),
                        tags: memory.tags.as_slice().to_vec(),
                        stored_at: moment(now),
                        changed_at: moment(now),
                    })
                })
                .map_err(write_error)
        })
        .await
    }

    /// The Memory `id` names, whole, where one is kept.
    pub(crate) async fn memory(&self, id: MemoryId) -> Result<Option<Memory>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("read Memory", move || {
            let mut connection = super::connect(&path)?;
            kept_memory(&mut connection, id).map_err(read_error)
        })
        .await
    }

    /// Changes the Memory `id` names as `change` says, changed as it lands,
    /// and answers it as it stands, where one is kept.
    pub(crate) async fn change_memory(
        &self,
        id: MemoryId,
        change: MemoryChange,
    ) -> Result<Option<Memory>, StorageError> {
        let path = self.database_path.clone();
        let clock = self.clock.clone();
        on_blocking_task("change Memory", move || {
            let mut connection = super::connect(&path)?;
            connection
                .immediate_transaction(|connection| {
                    let Some(kept) = kept_memory(connection, id)? else {
                        return Ok(None);
                    };
                    let now = landing_moment(connection, &clock)?;
                    let changed = Memory {
                        title: change
                            .title
                            .map_or(kept.title, |title| title.as_str().to_owned()),
                        body: change
                            .body
                            .map_or(kept.body, |body| body.as_str().to_owned()),
                        tags: change.tags.map_or(kept.tags, |tags| tags.into_vec()),
                        changed_at: moment(now),
                        ..kept
                    };
                    diesel::update(memories::table.find(id.get()))
                        .set((
                            memories::title.eq(&changed.title),
                            memories::body.eq(&changed.body),
                            memories::tags.eq(tags_column(&changed.tags)),
                            memories::changed_at.eq(now),
                        ))
                        .execute(connection)?;
                    Ok(Some(changed))
                })
                .map_err(write_error)
        })
        .await
    }

    /// Forgets the Memory `id` names; answers whether one was kept to forget.
    pub(crate) async fn forget_memory(&self, id: MemoryId) -> Result<bool, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("forget Memory", move || {
            let mut connection = super::connect(&path)?;
            connection
                .immediate_transaction(|connection| {
                    diesel::delete(memories::table.find(id.get()))
                        .execute(connection)
                        .map(|deleted| deleted == 1)
                })
                .map_err(write_error)
        })
        .await
    }

    /// The Memories `search` finds: with a query, those the full-text index
    /// matches, best match first, each with a snippet of its body around the
    /// words that matched; without one, every Memory, most recently changed
    /// first, each with its body's opening. Ties go to the Memory changed
    /// most recently, then to the one stored last. How many matched in all is
    /// counted in the same read, so it is never out of step with the rows.
    pub(crate) async fn search_memories(
        &self,
        search: MemorySearch,
    ) -> Result<FoundMemories, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("search Memories", move || {
            let mut connection = super::connect(&path)?;
            let mut conditions = Vec::new();
            let mut bound = Vec::new();
            let (from, snippet_of, order) = match &search.query {
                Some(query) => {
                    conditions.push("memories_fts MATCH ?");
                    bound.push(Bound::Text(query.as_str().to_owned()));
                    (
                        "memories_fts JOIN memories m ON m.id = memories_fts.rowid",
                        format!("snippet(memories_fts, 1, '', '', '…', {SNIPPET_TOKENS})"),
                        format!(
                            "bm25(memories_fts, {COLUMN_WEIGHTS}), m.changed_at DESC, m.id DESC"
                        ),
                    )
                }
                None => (
                    "memories m",
                    // One character past the bound, so a body running past it
                    // is cut with a mark saying so.
                    format!("substr(m.body, 1, {})", SNIPPET_CHARS + 1),
                    "m.changed_at DESC, m.id DESC".to_owned(),
                ),
            };
            for tag in search.tags.as_slice() {
                conditions
                    .push("EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?)");
                bound.push(Bound::Text(tag.clone()));
            }
            if let Some(after) = search.changed_after {
                conditions.push("m.changed_at >= ?");
                bound.push(Bound::Moment(millis(after)));
            }
            if let Some(before) = search.changed_before {
                conditions.push("m.changed_at < ?");
                bound.push(Bound::Moment(millis(before)));
            }
            let filter = if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.join(" AND "))
            };
            let limit = i64::try_from(search.limit).unwrap_or(i64::MAX);
            connection
                .transaction(|connection| {
                    let matched = binding(
                        format!("SELECT COUNT(*) AS value FROM {from}{filter}"),
                        &bound,
                    )
                    .get_result::<CountRow>(connection)?
                    .value;
                    let rows = binding(
                        format!(
                            "SELECT m.id AS id, m.title AS title, m.tags AS tags, m.stored_at AS \
                             stored_at, m.changed_at AS changed_at, {snippet_of} AS snippet FROM \
                             {from}{filter} ORDER BY {order} LIMIT {limit}"
                        ),
                        &bound,
                    )
                    .load::<FoundRow>(connection)?;
                    Ok(FoundMemories {
                        found: rows
                            .into_iter()
                            .map(|row| FoundMemory {
                                id: MemoryId::new(row.id),
                                tags: stored_tags(row.id, &row.tags),
                                title: row.title,
                                stored_at: moment(row.stored_at),
                                changed_at: moment(row.changed_at),
                                snippet: snippet(&row.snippet),
                            })
                            .collect(),
                        matched: usize::try_from(matched).unwrap_or(0),
                    })
                })
                .map_err(read_error)
        })
        .await
    }

    /// The titles of the `titles` Memories most recently changed, most recent
    /// first, and how many more there are.
    pub(crate) async fn memory_index(&self, titles: usize) -> Result<MemoryIndex, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("read the Memory index", move || {
            let mut connection = super::connect(&path)?;
            connection
                .transaction(|connection| {
                    let kept = memories::table.count().get_result::<i64>(connection)?;
                    let recent = memories::table
                        .order((memories::changed_at.desc(), memories::id.desc()))
                        .limit(i64::try_from(titles).unwrap_or(i64::MAX))
                        .select((memories::id, memories::title))
                        .load::<(i64, String)>(connection)?;
                    let older = usize::try_from(kept)
                        .unwrap_or(0)
                        .saturating_sub(recent.len());
                    Ok(MemoryIndex {
                        recent: recent
                            .into_iter()
                            .map(|(id, title)| IndexedMemory {
                                id: MemoryId::new(id),
                                title,
                            })
                            .collect(),
                        older,
                    })
                })
                .map_err(read_error)
        })
        .await
    }
}

/// `sql` with each of `bound` bound to its placeholders, in order.
fn binding(
    sql: String,
    bound: &[Bound],
) -> diesel::query_builder::BoxedSqlQuery<'_, Sqlite, diesel::query_builder::SqlQuery> {
    bound.iter().fold(
        diesel::sql_query(sql).into_boxed(),
        |query, value| match value {
            Bound::Text(text) => query.bind::<Text, _>(text.as_str()),
            Bound::Moment(moment) => query.bind::<BigInt, _>(*moment),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use diesel::connection::SimpleConnection;
    use futures_util::future::try_join_all;

    use super::*;
    use crate::memories::{Body, MatchQuery, Tags, Title};

    async fn repository(directory: &std::path::Path) -> StorageRepository {
        StorageRepository::open(directory)
            .await
            .expect("open the repository")
    }

    fn new(title: &str, body: &str, tags: &[&str]) -> NewMemory {
        NewMemory {
            title: Title::written(title).expect("a title"),
            body: Body::written(body).expect("a body"),
            tags: Tags::written(tags.iter().copied()).expect("tags"),
        }
    }

    /// A change setting a Memory's body to `body` and nothing else.
    fn rewriting(body: &str) -> MemoryChange {
        MemoryChange {
            title: None,
            body: Some(Body::written(body).expect("a body")),
            tags: None,
        }
    }

    fn searching(query: &str) -> MemorySearch {
        MemorySearch {
            query: MatchQuery::read(query).expect("a query"),
            tags: Tags::default(),
            changed_after: None,
            changed_before: None,
            limit: 10,
        }
    }

    async fn found(repository: &StorageRepository, query: &str) -> Vec<MemoryId> {
        repository
            .search_memories(searching(query))
            .await
            .expect("search")
            .found
            .into_iter()
            .map(|found| found.id)
            .collect()
    }

    /// Asks FTS5 whether its index agrees with the rows of the table it
    /// indexes, not only with itself: `rank` 1 compares the two.
    fn index_agrees_with_its_table(connection: &mut SqliteConnection) -> QueryResult<usize> {
        diesel::sql_query(
            "INSERT INTO memories_fts(memories_fts, rank) VALUES ('integrity-check', 1)",
        )
        .execute(connection)
    }

    /// The index answers what the table holds after every kind of write — a
    /// stored Memory is found by its words, a changed one by its new words
    /// and no longer by its old, a forgotten one by none — and after a write
    /// that rolled back; and FTS5 finds the index and the rows it indexes in
    /// agreement each time, as it does not once they part.
    #[tokio::test]
    async fn the_index_stays_in_step_with_every_write() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = repository(directory.path()).await;
        let mut connection = super::super::connect(&repository.database_path).expect("connect");

        let squashing = repository
            .store_memory(new("Squashing", "Never squash without asking.", &["git"]))
            .await
            .expect("store");
        let releases = repository
            .store_memory(new("Releases", "Tag a release on Fridays.", &["release"]))
            .await
            .expect("store");
        let passing = repository
            .store_memory(new("A passing thought", "Nothing worth keeping.", &[]))
            .await
            .expect("store");
        index_agrees_with_its_table(&mut connection).expect("in step once stored");
        assert_eq!(found(&repository, "squash").await, [squashing.id]);
        assert_eq!(
            found(&repository, "git").await,
            [squashing.id],
            "a tag is searched as words too"
        );

        repository
            .change_memory(squashing.id, rewriting("Rebase, never merge."))
            .await
            .expect("change")
            .expect("kept");
        assert!(repository.forget_memory(passing.id).await.expect("forget"));
        assert!(!repository.forget_memory(passing.id).await.expect("forget"));
        index_agrees_with_its_table(&mut connection).expect("in step once changed and forgotten");
        assert_eq!(found(&repository, "rebase").await, [squashing.id]);
        assert_eq!(
            found(&repository, "never asking").await,
            Vec::<MemoryId>::new(),
            "the old words are gone from the index"
        );
        assert_eq!(found(&repository, "passing").await, Vec::<MemoryId>::new());

        connection
            .batch_execute(&format!(
                "BEGIN IMMEDIATE; UPDATE memories SET body = 'Release on Mondays.' WHERE id = {}; \
                 DELETE FROM memories WHERE id = {}; INSERT INTO memories (title, body, tags, \
                 stored_at, changed_at) VALUES ('Rolled back', 'Never kept.', '[]', 0, 0); \
                 ROLLBACK;",
                releases.id, squashing.id
            ))
            .expect("write and roll back");
        index_agrees_with_its_table(&mut connection).expect("in step once a write rolled back");
        assert_eq!(found(&repository, "fridays").await, [releases.id]);
        assert_eq!(found(&repository, "rebase").await, [squashing.id]);
        assert_eq!(found(&repository, "kept").await, Vec::<MemoryId>::new());

        connection
            .batch_execute(
                "DROP TRIGGER memories_fts_update; UPDATE memories SET body = 'Words the index \
                 never read.';",
            )
            .expect("change rows behind the index's back");
        assert!(
            index_agrees_with_its_table(&mut connection).is_err(),
            "the check finds an index that disagrees with its table"
        );
    }

    /// A write reads the moment it is stamped with while it holds the
    /// database's write lock, so no write can land after another stamped
    /// later than it: the clock here tries to take the lock itself each time
    /// it is read, and finds it held.
    #[tokio::test]
    async fn a_writes_moment_is_read_while_it_holds_the_write_lock() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = repository(directory.path()).await;
        let path = repository.database_path.as_ref().clone();
        let readings = Arc::new(AtomicUsize::new(0));
        let unlocked = Arc::new(AtomicUsize::new(0));
        let clock = ServerClock::reading({
            let (readings, unlocked) = (readings.clone(), unlocked.clone());
            move || {
                readings.fetch_add(1, Ordering::SeqCst);
                // A connection with no busy timeout is refused a write lock
                // someone holds at once, rather than waiting for it.
                let mut probe = SqliteConnection::establish(path.to_str().expect("a UTF-8 path"))
                    .expect("open a probe");
                if probe.batch_execute("BEGIN IMMEDIATE").is_ok() {
                    unlocked.fetch_add(1, Ordering::SeqCst);
                    probe.batch_execute("ROLLBACK").expect("give the lock back");
                }
                SessionTimestamp(1_000)
            }
        });
        let repository = repository.with_clock(clock);

        let stored = repository
            .store_memory(new("Locked", "Stamped under the lock.", &[]))
            .await
            .expect("store");
        repository
            .change_memory(stored.id, rewriting("Changed under the lock."))
            .await
            .expect("change")
            .expect("kept");
        assert_eq!(
            readings.load(Ordering::SeqCst),
            2,
            "each write read the clock"
        );
        assert_eq!(
            unlocked.load(Ordering::SeqCst),
            0,
            "and read it while holding the write lock"
        );
    }

    /// Writes landing at once are stamped in the order they land, each later
    /// than every write before it, though the clock runs backwards a second
    /// at every reading; so the Memories most recently changed are those
    /// whose writes landed last. A store's identity is given as it lands, so
    /// it says the order stores landed in.
    #[tokio::test]
    async fn writes_landing_at_once_are_stamped_in_the_order_they_land_whatever_the_clock_says() {
        let directory = tempfile::tempdir().expect("create a data root");
        let ticks = Arc::new(AtomicU64::new(1_000_000));
        let clock = ServerClock::reading(move || {
            SessionTimestamp(ticks.fetch_sub(1_000, Ordering::SeqCst))
        });
        let repository = repository(directory.path()).await.with_clock(clock);

        let mut stored =
            try_join_all((0..8).map(|memory| {
                repository.store_memory(new(&format!("Memory {memory}"), "Kept.", &[]))
            }))
            .await
            .expect("store at once");
        stored.sort_by_key(|memory| memory.id);
        assert!(
            stored
                .windows(2)
                .all(|pair| pair[0].changed_at < pair[1].changed_at),
            "each store is stamped later than every store that landed before it: {stored:#?}"
        );
        assert!(
            stored
                .iter()
                .all(|memory| memory.stored_at == memory.changed_at)
        );

        let latest = stored.last().expect("stored").changed_at;
        let changed = try_join_all(
            stored[..3]
                .iter()
                .map(|memory| repository.change_memory(memory.id, rewriting("Changed."))),
        )
        .await
        .expect("change at once")
        .into_iter()
        .map(|memory| memory.expect("kept"))
        .collect::<Vec<_>>();
        let mut moments = changed
            .iter()
            .map(|memory| memory.changed_at)
            .collect::<Vec<_>>();
        moments.sort_unstable();
        moments.dedup();
        assert_eq!(moments.len(), 3, "no two changes share a moment");
        assert!(
            moments.iter().all(|moment| *moment > latest),
            "each change is stamped later than every write before it"
        );

        let mut most_recent = changed.iter().collect::<Vec<_>>();
        most_recent.sort_by_key(|memory| std::cmp::Reverse(memory.changed_at));
        let index = repository.memory_index(3).await.expect("the index");
        assert_eq!(
            index
                .recent
                .iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>(),
            most_recent
                .iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>(),
            "the Memories most recently changed are those changed last"
        );
        assert_eq!(index.older, 5);
    }

    #[tokio::test]
    async fn a_snippet_is_cut_from_the_body_whichever_column_matched() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = repository(directory.path()).await;
        let body = (0..200)
            .map(|word| format!("word{word}"))
            .collect::<Vec<_>>()
            .join(" ");
        repository
            .store_memory(new("Titled distinctly", &body, &[]))
            .await
            .expect("store");
        let found = repository
            .search_memories(searching("distinctly"))
            .await
            .expect("search")
            .found;
        assert!(
            found[0].snippet.starts_with("word0 word1") && found[0].snippet.ends_with('…'),
            "a match in the title alone answers the body's opening: {:?}",
            found[0].snippet
        );
        let found = repository
            .search_memories(searching("word150"))
            .await
            .expect("search")
            .found;
        assert!(
            found[0].snippet.contains("word150") && found[0].snippet.starts_with('…'),
            "{:?}",
            found[0].snippet
        );
    }
}
