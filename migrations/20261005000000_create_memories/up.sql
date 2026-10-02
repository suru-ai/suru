-- Memories: what a Sidekick chose to keep past its own Session, held by this
-- Server rather than by any Session or Provider, so no Session's deletion
-- touches one and none is read with a Session's history. A Memory is a short
-- title, a body, and the tags its Sidekick gave it -- a JSON array of strings,
-- kept normalised -- with the moments it was stored and last changed, in the
-- milliseconds every moment here is stored in.
--
-- Its identity is the row's own integer key, which is what a Sidekick names
-- it by: declared INTEGER PRIMARY KEY, so it is the rowid itself and no VACUUM
-- renumbers it, and AUTOINCREMENT, so no identity is ever given to another
-- Memory, a forgotten one's included.
CREATE TABLE memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT NOT NULL,
    stored_at BIGINT NOT NULL,
    changed_at BIGINT NOT NULL
);

-- Memories most recently changed first: the order a search with no query
-- lists them in, and the index a Sidekick's instructions are given.
CREATE INDEX memories_changed_at_idx ON memories(changed_at DESC, id DESC);

-- The full-text index a search reads, over each Memory's title, body and tags.
-- It holds no copy of their text: it reads the text from `memories` by rowid
-- whenever it needs it, as for a snippet. Words are folded to lower case and
-- stripped of their accents, and English ones reduced to their stem, so
-- "Reviewing" finds "reviews" and "cafe" finds "café".
CREATE VIRTUAL TABLE memories_fts USING fts5(
    title,
    body,
    tags,
    content = 'memories',
    content_rowid = 'id',
    tokenize = 'porter unicode61 remove_diacritics 2'
);

-- The index is kept in step with `memories` by these triggers rather than by
-- each write: every change to a row reaches it in the same statement, so no
-- path that writes a Memory can leave it behind. An external-content index
-- must be told a row's old text to remove it, which `old` holds.
CREATE TRIGGER memories_fts_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, body, tags)
    VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE TRIGGER memories_fts_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, old.tags);
END;

CREATE TRIGGER memories_fts_update AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, old.tags);
    INSERT INTO memories_fts(rowid, title, body, tags)
    VALUES (new.id, new.title, new.body, new.tags);
END;
