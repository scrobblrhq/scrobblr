-- =============================================================
--  Scrobblr — Scrobble classification (anti-botting, shadow mode)
--
--  The worker labels every scrobble counted / suspect / no_data after
--  ingest; nothing reads the labels yet. The unit of work is one user's
--  UTC day. Labels are sparse: a scrobble in a classified day without a
--  scrobble_flags row is counted, so reclassifying rewrites the small
--  tables below and only ever reads the (compressed) scrobbles hypertable.
-- =============================================================

-- What the client says the listener actually heard; ingest used to drop it.
-- scrobbles.duration_ms is the track length the client reported for the play.
ALTER TABLE scrobbles ADD COLUMN listened_ms INT;

-- MusicBrainz recording length, kept apart from tracks.duration_ms (set by
-- whichever client reported the track first) so the classifier can prefer it
-- without changing what any endpoint returns.
ALTER TABLE tracks ADD COLUMN mb_duration_ms INT;

CREATE TYPE scrobble_status AS ENUM ('counted', 'suspect', 'no_data');

-- Each distinct rule version + thresholds the worker has classified with.
CREATE TABLE classifier_rulesets (
    id           SERIAL       PRIMARY KEY,
    fingerprint  TEXT         NOT NULL UNIQUE,
    params       JSONB        NOT NULL,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE TABLE scrobble_classification_days (
    user_id          BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    day              DATE         NOT NULL,
    ruleset_id       INT          NOT NULL REFERENCES classifier_rulesets (id),
    -- Compared with user_activity_daily to spot days that changed since.
    scrobble_count   INT          NOT NULL,
    -- Scrobbles read from the previous day's last window: when that tail
    -- changes, this day is reclassified too.
    lookback_count   INT          NOT NULL,
    -- Later ids were ingested after this classification (view: unclassified).
    max_scrobble_id  BIGINT       NOT NULL,
    counted          INT          NOT NULL,
    suspect          INT          NOT NULL,
    no_data          INT          NOT NULL,
    classified_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (user_id, day)
);

CREATE INDEX idx_classification_days_ruleset ON scrobble_classification_days (ruleset_id);

-- Scrobbles that are not counted, and why.
CREATE TABLE scrobble_flags (
    scrobble_id      BIGINT           NOT NULL,
    played_at        TIMESTAMPTZ      NOT NULL,
    user_id          BIGINT           NOT NULL,
    day              DATE             NOT NULL,
    track_id         BIGINT           NOT NULL,
    status           scrobble_status  NOT NULL CHECK (status <> 'counted'),
    reason           TEXT             NOT NULL,   -- 'no_duration' | 'listening_budget'
    duration_source  TEXT,                        -- 'musicbrainz' | 'catalog' | 'reported'
    occupancy_ms     INT,                         -- listening time attributed to the scrobble
    load_ms          BIGINT,                      -- listening time in its window, itself included

    PRIMARY KEY (scrobble_id, played_at),
    FOREIGN KEY (user_id, day) REFERENCES scrobble_classification_days (user_id, day) ON DELETE CASCADE
);

CREATE INDEX idx_scrobble_flags_day   ON scrobble_flags (user_id, day);
CREATE INDEX idx_scrobble_flags_track ON scrobble_flags (track_id);

-- (user, day) pairs to (re)classify. Ingest inserts with ON CONFLICT DO
-- NOTHING, so a burst from one user costs an index probe per scrobble. No FK
-- to users: it would add a lookup and a row lock to that path.
CREATE TABLE classification_queue (
    user_id      BIGINT       NOT NULL,
    day          DATE         NOT NULL,
    priority     INT          NOT NULL DEFAULT 50,   -- 100 import/manual | 50 ingest | 10 sweep
    not_before   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    enqueued_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (user_id, day)
);

CREATE INDEX idx_classification_queue_due ON classification_queue (priority DESC, not_before);

-- One label per scrobble. status is NULL until the scrobble's day has been
-- classified with it included.
CREATE VIEW scrobble_labels AS
SELECT
    s.id          AS scrobble_id,
    s.played_at,
    s.user_id,
    CASE WHEN d.user_id IS NULL OR s.id > d.max_scrobble_id THEN NULL
         ELSE COALESCE(f.status, 'counted') END AS status,
    f.reason,
    d.ruleset_id,
    d.classified_at
FROM scrobbles s
LEFT JOIN scrobble_classification_days d
       ON d.user_id = s.user_id
      AND d.day = (s.played_at AT TIME ZONE 'UTC')::date
LEFT JOIN scrobble_flags f
       ON f.scrobble_id = s.id
      AND f.played_at = s.played_at;
