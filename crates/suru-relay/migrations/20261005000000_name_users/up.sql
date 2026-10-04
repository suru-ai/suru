-- The identity each user the operator's admission rules name was found to be,
-- as the Relay first started naming them (ADR-0048). A name given up may be
-- claimed by someone else, so it is looked up once and admitted as the
-- identity found from then on, for as long as the rules go on naming it.
CREATE TABLE named_users (
    provider TEXT NOT NULL,
    name TEXT NOT NULL,
    subject TEXT NOT NULL,
    named_at INTEGER NOT NULL,
    PRIMARY KEY (provider, name)
);
