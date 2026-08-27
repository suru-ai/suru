-- The Session whose Turn spawned this one, set exactly on a Subagent's child
-- Session. Every Session that predates this column is a root, which is what a
-- Session nothing spawned has always been. Deliberately no foreign key: the
-- background writer flushes dirty Sessions in no particular order, so a child
-- row may land before its parent's, and the subtree delete is the store's own
-- walk rather than a database cascade.
ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
