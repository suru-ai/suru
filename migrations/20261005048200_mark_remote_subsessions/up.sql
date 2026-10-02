-- Whether a Sidekick began the Session it acted on, for a Session on a
-- Remote: a Subsession there, heading its own tree, which only the
-- Sidekick's own Server knows began here, so a Client hiding Subsessions
-- finds the Sidekick's row to carry it by this. A Session of this Server's
-- own says so itself, and its rows leave this false.
ALTER TABLE sidekick_acts ADD COLUMN began BOOLEAN NOT NULL DEFAULT FALSE;

-- Whether the Session acted on is known to head its own tree there, for a
-- Session on a Remote: an act on a Subagent's Session stands by the Session
-- heading it, which the Remote is asked for; one it could not yet say is
-- kept under the Subagent's own identity, unresolved, and asked again. Only
-- an act known to stand by a top-level Session is ever taken as deleted for
-- missing from the Remote's listing of its top-level Sessions.
ALTER TABLE sidekick_acts ADD COLUMN resolved BOOLEAN NOT NULL DEFAULT TRUE;
