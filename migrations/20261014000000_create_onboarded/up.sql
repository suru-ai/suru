-- That this Server has been onboarded: a Client took its Onboarding, finishing
-- it or leaving it. The row's presence is the fact; its record says when, as
-- JSON, and a record that no longer decodes counts as no row at all. A fact
-- about this installation rather than a preference, so no Config Document
-- holds it.
CREATE TABLE onboarded (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    record TEXT NOT NULL
);
