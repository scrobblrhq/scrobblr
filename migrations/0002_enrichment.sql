-- =============================================================
--  newfm — Metadata enrichment
--  Adds enrichment bookkeeping to the catalog and a job queue
--  consumed by the worker binary.
-- =============================================================

-- When an entity was last successfully processed by the enrichment
-- pipeline (regardless of whether every field could be filled).
-- NULL = never enriched → eligible for ingest-time enqueue and backfill.
ALTER TABLE artists ADD COLUMN enriched_at TIMESTAMPTZ;
ALTER TABLE albums  ADD COLUMN enriched_at TIMESTAMPTZ;
ALTER TABLE tracks  ADD COLUMN enriched_at TIMESTAMPTZ;

-- Partial indexes so backfill sweeps and ingest-time guards stay cheap
-- even when the catalog grows.
CREATE INDEX idx_artists_unenriched ON artists (id) WHERE enriched_at IS NULL;
CREATE INDEX idx_albums_unenriched  ON albums  (id) WHERE enriched_at IS NULL;
CREATE INDEX idx_tracks_unenriched  ON tracks  (id) WHERE enriched_at IS NULL;


-- =============================================================
--  ENRICHMENT JOB QUEUE
--  One row per (entity_type, entity_id). Claimed by the worker with
--  FOR UPDATE SKIP LOCKED, so multiple workers are safe (the per-provider
--  rate limiter, however, is in-process — run one worker unless it moves
--  to Redis).
-- =============================================================

CREATE TABLE enrichment_jobs (
                                 id               BIGSERIAL       PRIMARY KEY,
                                 entity_type      TEXT            NOT NULL CHECK (entity_type IN ('artist', 'album', 'track')),
                                 entity_id        BIGINT          NOT NULL,
                                 status           TEXT            NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'running', 'done', 'failed')),
                                 priority         INT             NOT NULL DEFAULT 50,        -- 100 manual refresh | 50 ingest | 10 backfill
                                 attempts         INT             NOT NULL DEFAULT 0,
                                 force            BOOLEAN         NOT NULL DEFAULT FALSE,     -- manual refresh: overwrite provider-owned fields
                                 next_attempt_at  TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                                 last_error       TEXT,
                                 created_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                                 started_at       TIMESTAMPTZ,
                                 finished_at      TIMESTAMPTZ,

                                 UNIQUE (entity_type, entity_id)
);

-- Serves the claim query: pending jobs ordered by priority, then due time.
CREATE INDEX idx_enrichment_due ON enrichment_jobs (priority DESC, next_attempt_at)
    WHERE status = 'pending';
