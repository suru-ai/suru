-- A Session's own Settle: when the user set it aside as done for now, absent
-- for every Session that is active. The marker and the moment are one column
-- because a Session is settled exactly when there is a moment it was settled
-- at. Every Session that predates this column is active, which is what a
-- Session nobody has set aside has always been.
ALTER TABLE sessions ADD COLUMN settled_at BIGINT;
