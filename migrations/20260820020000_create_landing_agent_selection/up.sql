CREATE TABLE landing_agent_selection (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    selection TEXT NOT NULL
);
