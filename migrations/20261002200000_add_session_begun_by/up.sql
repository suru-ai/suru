-- Who began this Session on the user's behalf, where the user did not begin it
-- themselves: for a Subsession, the Sidekick whose Session began it, stored as
-- the author a Sidekick's Prompt names, so whoever else comes to begin a
-- Session for the user is one more kind of author rather than one more column.
-- Nullable: every Session the user began carries none, which is every Session
-- that predates this column.
ALTER TABLE sessions ADD COLUMN begun_by TEXT;
