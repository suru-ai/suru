-- An Account's id is never given again once its Account is removed, so one
-- id names one Account in the connection log for good, and nothing the Relay
-- counts by Account is counted against another that comes after it. SQLite
-- makes that promise of a table only as the table is made, so the Accounts
-- are made again, keeping every id, with the identities and Logins that refer
-- to them: each is copied whole before any of the old tables goes, the old
-- Accounts last, once nothing refers to them that removing them could take
-- with them. Renaming the new Accounts carries the references to them along.
CREATE TABLE accounts_kept (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
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
