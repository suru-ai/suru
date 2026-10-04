-- The Logins the operator has removed from the Relay's records — from its
-- command line, a process apart from the Relay, which may be running on them
-- all the while — each by its Server's identity key, whose connections and
-- joins the running Relay has yet to cut. Each removal is numbered, never
-- reusing a number, so the Relay can say which it has cut by forgetting them
-- here, and a removal made afterwards of the same key is never mistaken for
-- one it has cut. A Login formed again for the key forgets every removal of
-- it as it is formed, cutting what stood on the Login removed.
CREATE TABLE removed_logins (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    server_key BLOB NOT NULL,
    removed_at INTEGER NOT NULL
);

CREATE INDEX removed_logins_by_key ON removed_logins (server_key);
