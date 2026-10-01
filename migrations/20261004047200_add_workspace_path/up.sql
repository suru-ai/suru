-- The path a Workspace was presented by — its presented root — when its
-- Icon or Description last landed, so that a Workspace this Server holds
-- either for is known by where it is even once no Session works in it, or
-- before any has: described on the Landing, or by a Sidekick naming its
-- directory. A Session working there presents it afresh, so this is read
-- only for a Workspace no Session works in. Nullable: a row that predates it
-- says nowhere, and such a Workspace is known by its Sessions alone.
ALTER TABLE workspaces ADD COLUMN path TEXT;
