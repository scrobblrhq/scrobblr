-- =============================================================
--  Scrobblr — Scrobble classification (anti-botting, shadow mode)
--  Every scrobble is labeled after ingest by the worker's classifier
--  (counted / suspect / no_data + reason + the ruleset that produced
--  it). Labels live in their own tables rather than as columns on
--  `scrobbles` because:
--    * UPDATEs on compressed chunks (> 30 days) decompress the
--      affected user's segment, so reclassifying history would churn
--      decompression across the whole hypertable;
--    * every UPDATE on `scrobbles` writes continuous-aggregate
--      invalidations, re-materializing the charts on each pass;
--    * shadow mode must leave `scrobbles` and every query reading it
--      untouched.
--  Nothing outside the classifier reads these tables yet.
-- =============================================================

-- One row per (code rules version, thresholds). The worker upserts its
-- current config at startup, so changing either the code's
-- RULES_VERSION or any threshold yields a new id and the old labels
-- become "outdated". `activated_at` marks the ruleset the running
-- worker uses; the API's ingest hook reads the window size from the
-- latest-activated row (it has no classifier config of its own).
CREATE TABLE classification_rulesets (
    id             SERIAL          PRIMARY KEY,
    rules_version  INT             NOT NULL,
    params         JSONB           NOT NULL,
    created_at     TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    activated_at   TIMESTAMPTZ,

    UNIQUE (rules_version, params)
);


-- =============================================================
--  PER-SCROBBLE LABELS
--  No FK to `scrobbles` (hypertable composite key); rows whose
--  scrobble disappeared are removed when their day is reprocessed.
--  Uncompressed on purpose: reclassification rewrites rows here.
-- =============================================================

CREATE TABLE scrobble_classifications (
    scrobble_id    BIGINT          NOT NULL,
    played_at      TIMESTAMPTZ     NOT NULL,
    user_id        BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Lets a track's late duration fill find the user-days to re-check
    -- without scanning compressed scrobble chunks (no track index there).
    track_id       BIGINT          NOT NULL,
    status         TEXT            NOT NULL CHECK (status IN ('counted', 'suspect', 'no_data')),
    reason         TEXT            NOT NULL CHECK (reason IN ('ok', 'time_budget_exceeded', 'missing_duration')),
    ruleset_id     INT             NOT NULL REFERENCES classification_rulesets (id),
    score          REAL,                           -- listened / budget; NULL for no_data
    classified_at  TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

    PRIMARY KEY (scrobble_id, played_at)
);

SELECT create_hypertable('scrobble_classifications', 'played_at', chunk_time_interval => INTERVAL '7 days');

CREATE INDEX idx_classifications_user_time ON scrobble_classifications (user_id, played_at DESC);
CREATE INDEX idx_classifications_track     ON scrobble_classifications (track_id);


-- =============================================================
--  WORK LEDGER — one row per (user, UTC day)
--  The cheap way to find what needs (re)classifying: dirty rows are
--  the queue, `ruleset_id` says which ruleset last classified the day.
--
--  dirty_gen guards against losing a mark that arrives while the day
--  is being processed: every mark bumps it, and completion clears
--  `dirty` only if the gen still equals the value read at claim time.
--
--  next_attempt_at is "not before": the worker claims a row once
--  next_attempt_at + its settle delay has passed (the settle delay is
--  worker config, so it is applied at claim time rather than here).
-- =============================================================

CREATE TABLE classification_days (
    user_id          BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    day              TIMESTAMPTZ     NOT NULL,       -- time_bucket('1 day', played_at), UTC midnight
    dirty            BOOLEAN         NOT NULL DEFAULT TRUE,
    dirty_gen        BIGINT          NOT NULL DEFAULT 1,
    priority         INT             NOT NULL DEFAULT 50,   -- 100 manual reclassify | 50 ingest | 10 background
    next_attempt_at  TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    claimed_at       TIMESTAMPTZ,                    -- lease; an expired lease is claimable again
    attempts         INT             NOT NULL DEFAULT 0,
    last_error       TEXT,
    ruleset_id       INT             REFERENCES classification_rulesets (id),
    classified_at    TIMESTAMPTZ,
    counted          INT             NOT NULL DEFAULT 0,
    suspect          INT             NOT NULL DEFAULT 0,
    no_data          INT             NOT NULL DEFAULT 0,

    PRIMARY KEY (user_id, day)
);

-- Serves the claim query.
CREATE INDEX idx_classification_days_due     ON classification_days (priority DESC, next_attempt_at)
    WHERE dirty;
-- Serves the outdated-ruleset sweep and the report.
CREATE INDEX idx_classification_days_ruleset ON classification_days (ruleset_id);


-- =============================================================
--  TRACK DURATION FILLS
--  A track's duration goes NULL → value later (MusicBrainz enrichment,
--  or another client reporting it at ingest). Its scrobbles were
--  labeled no_data meanwhile, so the fill is queued here and the
--  worker re-dirties the affected user-days. A trigger catches both
--  fill paths; neither caller can tell on its own that it filled.
-- =============================================================

CREATE TABLE classification_track_refresh (
    track_id     BIGINT          PRIMARY KEY,
    enqueued_at  TIMESTAMPTZ     NOT NULL DEFAULT NOW()
);

-- Runs inside the ingest / enrichment write. The EXCEPTION block makes
-- a failure here a WARNING instead of aborting the track upsert:
-- shadow-mode classification must never reject a scrobble.
CREATE OR REPLACE FUNCTION enqueue_classification_track_refresh()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    BEGIN
        INSERT INTO classification_track_refresh (track_id)
        VALUES (NEW.id)
        ON CONFLICT (track_id) DO NOTHING;
    EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'classification: track refresh enqueue failed for track %: %', NEW.id, SQLERRM;
    END;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_tracks_duration_filled
    AFTER UPDATE OF duration_ms ON tracks
    FOR EACH ROW
    WHEN (OLD.duration_ms IS NULL AND NEW.duration_ms IS NOT NULL)
    EXECUTE FUNCTION enqueue_classification_track_refresh();
