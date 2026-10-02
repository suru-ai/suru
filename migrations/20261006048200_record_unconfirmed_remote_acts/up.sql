-- Whether an act on a Remote's Session is known to have been done there. One
-- carried to the Remote whose answer never came back whole may have been
-- done all the same, so it is kept, unconfirmed, until a read of that Remote
-- shows the Session it named — confirming it — or shows, asked for after the
-- act's outcome became unknown, that the Remote holds no such Session. An act
-- of this Server's own, and every act a Remote answered, is confirmed.
ALTER TABLE sidekick_acts ADD COLUMN confirmed BOOLEAN NOT NULL DEFAULT TRUE;

-- The key fingerprint of the Pairing an act on a Remote's Session was carried
-- through: only a read through that same Pairing confirms or drops it, and a
-- name paired anew to another key takes the acts carried to the old one with
-- it. Empty for an act of this Server's own.
ALTER TABLE sidekick_acts ADD COLUMN pairing TEXT NOT NULL DEFAULT '';

-- For a beginning on a Remote not yet confirmed, what it asks for, as JSON:
-- the Worktree preparation and the creation, by the identities chosen before
-- either was first asked — the Session's own, its first Prompt's and the
-- preparation's — and how far it got, so asking again is the very same
-- request and never another beginning. Cleared once the beginning is
-- confirmed.
ALTER TABLE sidekick_acts ADD COLUMN beginning TEXT;
