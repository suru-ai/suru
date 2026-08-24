-- A pure column addition: Sessions created before Title derivation keep the
-- Titles their first Prompts gave them and are never back-filled.
ALTER TABLE sessions ADD COLUMN emoji TEXT;
