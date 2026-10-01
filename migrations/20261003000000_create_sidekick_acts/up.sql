-- The Sessions each Sidekick has acted on -- begun, sent a Prompt, answered,
-- interrupted, set aside, or brought back; reading one is no act -- with the
-- moment of its latest act on each, which is what the Sidekick's Session's
-- tree lists beneath it and orders them by. Keyed by the Sidekick's Session,
-- deleted with it, and by the Session acted on together with its Origin,
-- since a Session's identity is only unique within its Origin: empty for one
-- of this Server's own, and otherwise the name of the Remote it lives on. The
-- Session acted on holds no foreign key, because a Remote's is not stored
-- here; deleting one of this Server's own deletes its rows here with it.
CREATE TABLE sidekick_acts (
    sidekick_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    origin TEXT NOT NULL,
    session_id TEXT NOT NULL,
    acted_at BIGINT NOT NULL,
    PRIMARY KEY (sidekick_session_id, origin, session_id)
);

CREATE INDEX sidekick_acts_session_idx ON sidekick_acts(origin, session_id);
