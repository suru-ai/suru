ALTER TABLE sessions ADD COLUMN revision BIGINT NOT NULL DEFAULT 1;

CREATE TABLE prompts (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    row_order BIGINT NOT NULL,
    admission_order BIGINT NOT NULL,
    payload TEXT NOT NULL
);

CREATE INDEX prompts_session_order_idx ON prompts(session_id, row_order);

CREATE TABLE turns (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    prompt_id TEXT NOT NULL,
    row_order BIGINT NOT NULL,
    payload TEXT NOT NULL
);

CREATE INDEX turns_session_order_idx ON turns(session_id, row_order);

CREATE TABLE messages (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    row_order BIGINT NOT NULL,
    transcript_order BIGINT NOT NULL,
    payload TEXT NOT NULL
);

CREATE INDEX messages_session_order_idx ON messages(session_id, row_order);
CREATE UNIQUE INDEX messages_transcript_order_idx
    ON messages(session_id, transcript_order);

CREATE TABLE activities (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    row_order BIGINT NOT NULL,
    transcript_order BIGINT NOT NULL,
    payload TEXT NOT NULL
);

CREATE INDEX activities_session_order_idx ON activities(session_id, row_order);
CREATE UNIQUE INDEX activities_transcript_order_idx
    ON activities(session_id, transcript_order);
