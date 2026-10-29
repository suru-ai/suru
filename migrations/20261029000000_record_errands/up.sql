-- Errands: Suru's own one-shot Provider calls, each recorded as it ends once
-- it has reached a Provider (ADR 0057). An Errand belongs to no Session, so no
-- row names one and no Session's deletion touches one. A row says what the
-- Errand was for, the Provider and Model it ran at, when it began and ended
-- in the milliseconds every moment here is stored in, and how it ended.
--
-- What it consumed is each absent where unknown, never a fabricated zero: its
-- Usage as the protocol's JSON, and its Cost in billionths of a dollar beside
-- the Cost Basis that computed it, the two known or unknown together.
--
-- Rows are read by the interval they began in, never loaded at startup, so
-- they are indexed by when they began.
CREATE TABLE errands (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    purpose TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    began_at BIGINT NOT NULL,
    ended_at BIGINT NOT NULL,
    outcome TEXT NOT NULL,
    usage TEXT,
    cost_nano_usd BIGINT,
    cost_basis TEXT,
    CHECK ((cost_nano_usd IS NULL) = (cost_basis IS NULL))
);

CREATE INDEX errands_by_began_at ON errands (began_at);
