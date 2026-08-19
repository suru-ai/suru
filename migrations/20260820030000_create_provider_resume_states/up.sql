CREATE TABLE provider_resume_states (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (session_id, provider)
);
