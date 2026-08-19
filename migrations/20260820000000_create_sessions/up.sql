CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    workspace TEXT NOT NULL,
    agent_selection TEXT,
    agent_selection_availability TEXT NOT NULL,
    status TEXT NOT NULL
);

CREATE INDEX sessions_updated_at_idx ON sessions(updated_at DESC);
