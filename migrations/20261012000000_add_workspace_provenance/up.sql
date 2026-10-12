-- The workspaces table is the Workspace Registry (ADR 0051): an entry for
-- every Workspace this Server has known, which records how it came to be
-- known, as JSON — "session" where a Session began there, or was found
-- working there when its directory was resolved again at start, and
-- "description" where someone described it, or chose its Icon, before any
-- Session had. Ways of coming to know a Workspace that carry more than
-- their name, such as discovery under a Workspace Root, are spelled as an
-- object naming the way and what it carries.
--
-- Every row that predates it was made by an Icon or a Description landing,
-- so it was known by a Session where a Session is grouped under it now, and
-- by its Description otherwise. A Session whose metadata no longer reads as
-- JSON says nothing of where it works.
ALTER TABLE workspaces ADD COLUMN provenance TEXT NOT NULL DEFAULT '"description"';

UPDATE workspaces
SET provenance = '"session"'
WHERE id IN (
    SELECT json_extract(workspace, '$.workspace.id')
    FROM sessions
    WHERE json_valid(workspace)
);
