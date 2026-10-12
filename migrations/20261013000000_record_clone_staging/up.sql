-- The directories this Server's Clones are made in: each recorded, with the
-- Server run whose Clone made it, before it is made, and forgotten once it is
-- gone. What a Clone of a run that died left behind is what the next start
-- removes -- a directory recorded here, still marked as the run that
-- recorded it -- and nothing else, however it is named.
CREATE TABLE clone_staging (
    directory TEXT PRIMARY KEY NOT NULL,
    run TEXT NOT NULL
);
