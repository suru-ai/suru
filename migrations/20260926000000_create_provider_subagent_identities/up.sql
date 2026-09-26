-- The Provider's own identity for the Subagent a child Session is -- Claude's
-- task id, Codex's child thread id -- stored with that Session when the Subagent
-- spawns, so a resume the Provider names after Suru restarts still finds the
-- Session it continues (ADR 0031). Keyed by the child Session and deleted with
-- it. The Session row is written first in the same transaction, which is what
-- lets this one hold a foreign key the parent link cannot.
CREATE TABLE provider_subagent_identities (
    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    subagent_id TEXT NOT NULL
);
