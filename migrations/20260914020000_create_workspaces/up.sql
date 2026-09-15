-- The first Workspace-owned durable state: its Icon alone, keyed by the
-- serialized WorkspaceId. Deliberately not a broader repository registry
-- (ADR 0027) -- a Workspace's other facts are still resolved fresh, never
-- cached here.
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY NOT NULL,
    icon TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
