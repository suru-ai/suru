-- The identity keys of the Logins the operator has removed from the Relay's
-- records — from its command line, a process apart from the Relay, which
-- may be running on them all the while — whose connections and joins the
-- running Relay has yet to cut. The Relay cuts what stands on each, and then
-- forgets it here; a Login formed again for the key before it does forgets it
-- here as it is formed, cutting what stood on the one removed.
CREATE TABLE removed_logins (
    server_key BLOB PRIMARY KEY NOT NULL,
    removed_at INTEGER NOT NULL
);
