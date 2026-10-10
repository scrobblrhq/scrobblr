-- =============================================================
--  Scrobblr — Cross-account coordination (anti-botting, shadow mode)
--
--  A periodic worker job looks for accounts that push the same artists
--  and tracks together (shared::coordination) and gives each a factor from
--  0 to 1 that global rankings multiply its listener credits and weights
--  by. Profiles never read it, and nothing is blocked, deleted or
--  relabelled.
-- =============================================================

-- Where an account registered from, as keyed hashes (shared::origin) of
-- the address and its network, never the address. Written at registration
-- while REGISTRATION_HASH_KEY is set; the worker deletes rows older than
-- REGISTRATION_ORIGIN_DAYS. Values made under different keys (key_id)
-- never compare equal.
CREATE TABLE registration_origins (
    user_id     BIGINT       PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    key_id      INT          NOT NULL,
    address     BIGINT       NOT NULL,
    network     BIGINT       NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_registration_origins_created ON registration_origins (created_at);

-- One row per detection run.
CREATE TABLE coordination_runs (
    id           SERIAL       PRIMARY KEY,
    fingerprint  TEXT         NOT NULL,
    -- report: cohorts are listed, every factor stays 1; apply: rankings
    -- apply the factors.
    mode         TEXT         NOT NULL CHECK (mode IN ('report', 'apply')),
    window_from  DATE         NOT NULL,
    window_to    DATE         NOT NULL,
    -- Active accounts, groups examined, accounts with a factor below 1.
    accounts     INT          NOT NULL,
    groups       INT          NOT NULL,
    flagged      INT          NOT NULL,
    cohorts      INT          NOT NULL,
    -- Fewer active accounts than the minimum: nothing was judged.
    too_small    BOOLEAN      NOT NULL,
    started_at   TIMESTAMPTZ  NOT NULL,
    took_ms      INT          NOT NULL,
    finished_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Flagged accounts grouped by the artist or track of their highest
-- suspicion. Thousandths throughout; the signals are means over members.
CREATE TABLE coordination_cohorts (
    run_id     INT     NOT NULL REFERENCES coordination_runs (id) ON DELETE CASCADE,
    cohort     INT     NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('artist', 'track')),
    entity_id  BIGINT  NOT NULL,
    members    INT     NOT NULL,
    score      INT     NOT NULL,
    factor     INT     NOT NULL,
    age        INT     NOT NULL,
    origin     INT     NOT NULL,
    client     INT     NOT NULL,
    sync       INT     NOT NULL,
    shape      INT     NOT NULL,
    burst      INT     NOT NULL,
    -- What fired (registration windows, origin and client cells, lockstep
    -- days) and the cohort's top targets.
    detail     JSONB   NOT NULL,

    PRIMARY KEY (run_id, cohort)
);

CREATE TABLE coordination_accounts (
    run_id     INT     NOT NULL REFERENCES coordination_runs (id) ON DELETE CASCADE,
    user_id    BIGINT  NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    cohort     INT     NOT NULL,
    suspicion  INT     NOT NULL,
    factor     INT     NOT NULL,
    age        INT     NOT NULL,
    origin     INT     NOT NULL,
    client     INT     NOT NULL,
    sync       INT     NOT NULL,
    shape      INT     NOT NULL,
    burst      INT     NOT NULL,

    PRIMARY KEY (run_id, user_id),
    FOREIGN KEY (run_id, cohort) REFERENCES coordination_cohorts (run_id, cohort)
        ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);

-- The factor of each (user, day) below 1, as the latest run covering the
-- day judged it: a run rewrites its window's days and leaves earlier days
-- as runs left them, so a farm that stopped keeps its days dented in the
-- year's ranking. Rankings weigh a user's days' factors by ranking_days
-- into one factor per period; kept as long as the weights.
CREATE TABLE coordination_days (
    user_id  BIGINT  NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    day      DATE    NOT NULL,
    factor   INT     NOT NULL CHECK (factor >= 0 AND factor < 1000),
    run_id   INT     NOT NULL,

    PRIMARY KEY (user_id, day)
);

-- The run whose factors a snapshot applied; a newer one makes it due.
ALTER TABLE ranking_snapshots ADD COLUMN coordination_run_id INT;
