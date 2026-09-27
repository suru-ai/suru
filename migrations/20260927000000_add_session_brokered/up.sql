-- Whether this Session is a brokered Subagent's: one Suru spawned through the
-- Broker, on a Provider the delegating Agent chose, rather than one the
-- delegating Agent's own Provider spawned. A brokered Subagent's conversation
-- runs on a Provider actor of its own instead of riding its spawner's (ADR
-- 0035), so this is what routes its Provider work after a restart. Every
-- Session that predates this column is a top-level Session or a native
-- Subagent's, which is what false says.
ALTER TABLE sessions ADD COLUMN brokered BOOLEAN NOT NULL DEFAULT FALSE;
