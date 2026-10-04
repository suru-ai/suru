-- The identity each user the operator's admission rules have named was found
-- to be, as the Relay first started naming them (ADR-0048). A name given up
-- may be claimed by someone else, so it is looked up once and admitted as the
-- identity found ever after, kept while the rules stop naming it too.
CREATE TABLE named_users (
    provider TEXT NOT NULL,
    name TEXT NOT NULL,
    subject TEXT NOT NULL,
    named_at INTEGER NOT NULL,
    PRIMARY KEY (provider, name)
);
