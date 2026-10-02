-- What an act on a Remote's Session not yet confirmed left there to be found
-- by, as a JSON list: a Prompt by the identity this Server chose for it, or
-- an Answer by the act this Server named it as. Only a read finding one of
-- them, as this Peer's, confirms the act, and one asked for after it that
-- finds none finds it was never done. Empty for an act that left nothing to
-- be found by — interrupting, setting aside — which the Session being found
-- confirms, and for every confirmed act.
ALTER TABLE sidekick_acts ADD COLUMN evidence TEXT NOT NULL DEFAULT '[]';
