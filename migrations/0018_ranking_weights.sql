-- =============================================================
--  Scrobblr — Ranking weights (anti-botting, shadow mode)
--
--  Each scrobble gets a weight from 0 to 1 for global rankings
--  (shared::ranking), computed per user and UTC day after the day is
--  classified. Stored summed per (user, day, track): rankings over any
--  period read these rows, never the scrobbles hypertable, whose
--  compressed chunks are segmented by user. Labels are left as they are;
--  nothing user-facing reads these tables yet.
-- =============================================================

-- Each distinct weighting version + params the worker has used.
CREATE TABLE ranking_rulesets (
    id           SERIAL       PRIMARY KEY,
    fingerprint  TEXT         NOT NULL UNIQUE,
    params       JSONB        NOT NULL,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE TABLE ranking_days (
    user_id         BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    day             DATE         NOT NULL,
    ruleset_id      INT          NOT NULL REFERENCES ranking_rulesets (id),
    -- scrobble_classification_days.classified_at the weights were made
    -- from: a newer classification makes them stale.
    classified_at   TIMESTAMPTZ  NOT NULL,
    plays           INT          NOT NULL,
    -- Plays that kept some weight, and the weights' sum in thousandths.
    weighted        INT          NOT NULL,
    weight          BIGINT       NOT NULL,
    -- Plays each reason applied to (shared::ranking::Reason); a play can
    -- have several.
    unclassified    INT          NOT NULL,
    suspect         INT          NOT NULL,
    duplicate       INT          NOT NULL,
    no_data         INT          NOT NULL,
    imported        INT          NOT NULL,
    new_account     INT          NOT NULL,
    short_listen    INT          NOT NULL,
    no_listened     INT          NOT NULL,
    unknown_client  INT          NOT NULL,
    track_cap       INT          NOT NULL,
    artist_cap      INT          NOT NULL,
    computed_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (user_id, day)
);

CREATE INDEX idx_ranking_days_ruleset ON ranking_days (ruleset_id);

-- One row per (user, day, track) with scrobbles: all of them (raw rankings)
-- and their weights (filtered rankings). artist_id is the primary artist,
-- as in every aggregate.
CREATE TABLE ranking_daily (
    user_id    BIGINT  NOT NULL,
    day        DATE    NOT NULL,
    track_id   BIGINT  NOT NULL,
    artist_id  BIGINT  NOT NULL,
    plays      INT     NOT NULL,
    weight     INT     NOT NULL,

    PRIMARY KEY (user_id, day, track_id),
    FOREIGN KEY (user_id, day) REFERENCES ranking_days (user_id, day) ON DELETE CASCADE
);

-- A period's rankings read every row of its days.
CREATE INDEX idx_ranking_daily_day ON ranking_daily (day);

-- (user, day) pairs to weigh, queued when their classification is written
-- and by the worker's sweep.
CREATE TABLE ranking_queue (
    user_id      BIGINT       NOT NULL,
    day          DATE         NOT NULL,
    enqueued_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (user_id, day)
);

CREATE INDEX idx_ranking_queue_enqueued ON ranking_queue (enqueued_at);
