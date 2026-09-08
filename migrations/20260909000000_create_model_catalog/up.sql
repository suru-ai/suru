CREATE TABLE model_catalog (
    provider TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL,
    discovered_at BIGINT NOT NULL
);
