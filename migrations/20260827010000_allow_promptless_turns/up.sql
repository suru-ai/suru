-- A Turn's Prompt reference becomes optional: a Subagent Session's Turn is
-- opened by its spawn rather than by a Prompt's delivery, so it has none.
-- SQLite cannot drop NOT NULL in place, so the table is rebuilt around the
-- loosened column with every row carried over.
CREATE TABLE turns_with_optional_prompt (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    prompt_id TEXT,
    row_order BIGINT NOT NULL,
    payload TEXT NOT NULL
);

INSERT INTO turns_with_optional_prompt SELECT * FROM turns;
DROP TABLE turns;
ALTER TABLE turns_with_optional_prompt RENAME TO turns;

CREATE INDEX turns_session_order_idx ON turns(session_id, row_order);
