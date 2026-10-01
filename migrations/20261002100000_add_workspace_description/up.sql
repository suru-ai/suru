-- A Workspace's Description, kept beside its Icon: a sentence or two saying
-- what the Workspace is for, derived in the same Errand as the Icon or set by
-- the user or a Sidekick. Nullable: absent until one lands, and absent again
-- once a set one is cleared. `description_set` says whether it was set rather
-- than derived, which is what lets it stand against every later derivation;
-- every row that predates these columns carries no Description, which is what
-- false says of it.
ALTER TABLE workspaces ADD COLUMN description TEXT;
ALTER TABLE workspaces ADD COLUMN description_set BOOLEAN NOT NULL DEFAULT FALSE;
