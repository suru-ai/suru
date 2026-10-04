-- When an Account was last logged in as, from any of its Servers, which an
-- operator may require be recent; and when it lapsed, where it has — its
-- Logins refused until one of its Servers logs in afresh, and nothing of it
-- forgotten (ADR-0048).
ALTER TABLE accounts ADD COLUMN logged_in_at INTEGER NOT NULL DEFAULT 0;
UPDATE accounts SET logged_in_at = COALESCE(
    (SELECT MAX(formed_at) FROM logins WHERE logins.account_id = accounts.id),
    created_at
);
ALTER TABLE accounts ADD COLUMN lapsed_at INTEGER;
