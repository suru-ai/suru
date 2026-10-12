-- The workspaces table becomes the Workspace Registry (ADR 0051): an entry
-- for every Workspace this Server has known, listed from it alone. Each
-- entry records how it came to be known, as JSON — "session" where a
-- Session began there, or was found working there when its directory was
-- resolved again, and "description" where someone described it, or chose its
-- Icon, before any Session had; ways that carry more than their name, such
-- as discovery under a Workspace Root, are spelled as an object naming the
-- way and what it carries. Each also records, as JSON, how its path presents
-- it: "directory" outside source control, and otherwise "main_root",
-- "bare_root" or "repository_metadata", where its Repository's main checkout
-- is not known — which is what lets the entry be named as its Sessions name
-- it without reading its Repository again. Null says the entry's
-- presentation is not known.
ALTER TABLE workspaces ADD COLUMN provenance TEXT NOT NULL DEFAULT '"description"';
ALTER TABLE workspaces ADD COLUMN presentation TEXT;

-- Every Workspace a stored Session names, as the copy of its most recently
-- updated Session presents it, beside when the first of them was made. A
-- Session whose metadata no longer reads as JSON says nothing of where it
-- works; one Suru cannot otherwise read still does.
CREATE TEMPORARY TABLE presented_by_sessions AS
SELECT id, path, presentation, first_seen, last_seen
FROM (
    SELECT
        json_extract(workspace, '$.workspace.id') AS id,
        json_extract(workspace, '$.workspace.path') AS path,
        CASE
            WHEN coalesce(json_type(workspace, '$.workspace.repository'), 'null') = 'null'
                THEN '"directory"'
            WHEN json_extract(workspace, '$.workspace.repository.location.kind') = 'main'
                THEN '"main_root"'
            WHEN json_extract(workspace, '$.workspace.repository.location.kind') = 'bare'
                THEN '"bare_root"'
            WHEN json_extract(workspace, '$.workspace.repository.location.kind') = 'unknown_main'
                THEN '"repository_metadata"'
        END AS presentation,
        min(created_at) OVER named AS first_seen,
        updated_at AS last_seen,
        row_number() OVER (named ORDER BY updated_at DESC) AS newest
    FROM sessions
    WHERE json_valid(workspace)
    WINDOW named AS (PARTITION BY json_extract(workspace, '$.workspace.id'))
)
WHERE newest = 1 AND id IS NOT NULL AND path IS NOT NULL;

-- A row that predates the Registry was made by an Icon or a Description
-- landing. Where a Session is grouped under it, it was known by that Session
-- and is presented as that Session presents it, which is how it was listed;
-- otherwise it was known by its Description, and only a plain directory's
-- presentation can be told from its identity alone.
UPDATE workspaces
SET provenance = '"session"',
    path = (SELECT path FROM presented_by_sessions WHERE presented_by_sessions.id = workspaces.id),
    presentation = (
        SELECT presentation FROM presented_by_sessions
        WHERE presented_by_sessions.id = workspaces.id
    )
WHERE id IN (SELECT id FROM presented_by_sessions);

UPDATE workspaces
SET presentation = '"directory"'
WHERE presentation IS NULL AND id LIKE 'directory:%';

-- A Workspace a Session works in that no Icon or Description ever landed
-- for had no row; it is entered now, as its Sessions present it, so it is
-- listed from the first start rather than once its Sessions are resolved
-- again.
INSERT INTO workspaces (
    id, icon, created_at, updated_at, description, description_set, path, provenance,
    presentation
)
SELECT id, NULL, first_seen, last_seen, NULL, FALSE, path, '"session"', presentation
FROM presented_by_sessions
WHERE id NOT IN (SELECT id FROM workspaces);

DROP TABLE presented_by_sessions;
