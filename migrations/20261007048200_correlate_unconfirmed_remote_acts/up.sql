-- An act on a Remote's Session not yet confirmed that began no Session,
-- held before what it left there was kept, left something or nothing no one
-- can now tell, so no read can judge it: it is let go of, rather than have
-- its Session merely standing confirm it. A beginning names its Session
-- beforehand, and is kept.
DELETE FROM sidekick_acts WHERE confirmed = 0 AND began = 0;

-- What a read of a Remote must find to confirm an act there not yet
-- confirmed, as JSON: "session_standing" — the Session it named, all that
-- an act leaving nothing behind (interrupting, setting aside or bringing
-- back) or a beginning needs, and all a confirmed act ever did — or
-- {"left": [...]}, what it left there: a Prompt by the identity this Server
-- chose for it, or an Answer by the act this Server named it as. Only a
-- read finding one of those, as this Peer's, confirms it, and one asked
-- for after it that finds none finds it was never done.
ALTER TABLE sidekick_acts ADD COLUMN evidence TEXT NOT NULL DEFAULT '"session_standing"';
