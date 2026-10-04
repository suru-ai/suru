CREATE TABLE accounts_kept (
    id INTEGER PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL,
    logged_in_at INTEGER NOT NULL DEFAULT 0,
    lapsed_at INTEGER
);
INSERT INTO accounts_kept (id, created_at, logged_in_at, lapsed_at)
    SELECT id, created_at, logged_in_at, lapsed_at FROM accounts;

CREATE TABLE identities_kept (
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    username TEXT NOT NULL,
    account_id INTEGER NOT NULL REFERENCES accounts_kept (id) ON DELETE CASCADE,
    PRIMARY KEY (provider, subject)
);
INSERT INTO identities_kept (provider, subject, username, account_id)
    SELECT provider, subject, username, account_id FROM identities;

CREATE TABLE logins_kept (
    server_key BLOB PRIMARY KEY NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    account_id INTEGER NOT NULL REFERENCES accounts_kept (id) ON DELETE CASCADE,
    hostname TEXT NOT NULL,
    formed_at INTEGER NOT NULL
);
INSERT INTO logins_kept (server_key, fingerprint, account_id, hostname, formed_at)
    SELECT server_key, fingerprint, account_id, hostname, formed_at FROM logins;

DROP INDEX logins_by_account;
DROP TABLE logins;
DROP TABLE identities;
DROP TABLE accounts;
ALTER TABLE accounts_kept RENAME TO accounts;
ALTER TABLE identities_kept RENAME TO identities;
ALTER TABLE logins_kept RENAME TO logins;
CREATE INDEX logins_by_account ON logins (account_id);
