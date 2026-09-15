-- A Session's Icon, derived alongside its Title through the Title Errand and
-- carried as its Icon Catalog name rather than a codepoint (see ADR 0028).
-- Nullable: absent until a derivation lands one, and forever absent for a
-- Session whose derivation was skipped, failed, or predates this column.
ALTER TABLE sessions ADD COLUMN icon TEXT;
