-- An Account is the Relay's own record, apart from the identity that logs in
-- as it (ADR-0048), so a user linking identities by hand can be added later.
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL
);

-- An identity at an identity provider, keyed by the provider and the stable
-- subject id it gives, never by name or email. Each answers to one Account,
-- and no two are ever linked automatically.
CREATE TABLE identities (
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    username TEXT NOT NULL,
    account_id INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    PRIMARY KEY (provider, subject)
);

-- A Server's lasting standing under one Account, tied to the identity key it
-- proves each time it connects. It carries no expiry: it stands until its
-- Server's user or the Relay's operator removes it.
CREATE TABLE logins (
    server_key BLOB PRIMARY KEY NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    account_id INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    hostname TEXT NOT NULL,
    formed_at INTEGER NOT NULL
);

CREATE INDEX logins_by_account ON logins (account_id);
