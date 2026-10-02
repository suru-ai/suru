-- Whether a Sidekick began the Session it acted on, for a Session on a
-- Remote: a Subsession there, heading its own tree, which only the
-- Sidekick's own Server knows began here, so a Client hiding Subsessions
-- finds the Sidekick's row to carry it by this. A Session of this Server's
-- own says so itself, and its rows leave this false.
ALTER TABLE sidekick_acts ADD COLUMN began BOOLEAN NOT NULL DEFAULT FALSE;
