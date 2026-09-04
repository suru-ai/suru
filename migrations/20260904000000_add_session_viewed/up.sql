-- The last moment any Client reported this Session open in its main view.
-- Sessions predating Viewed have not yet been seen under this reading.
ALTER TABLE sessions ADD COLUMN viewed_at BIGINT;
