-- The organization each organization the operator's admission rules have
-- named was found to be, by its identity provider's stable id for it, as the
-- Relay first started naming it (ADR-0048). A name given up may be claimed by
-- another organization, so its members are admitted by the organization
-- found ever after, kept while the rules stop naming it too.
CREATE TABLE named_organizations (
    provider TEXT NOT NULL,
    name TEXT NOT NULL,
    organization TEXT NOT NULL,
    named_at INTEGER NOT NULL,
    PRIMARY KEY (provider, name)
);
