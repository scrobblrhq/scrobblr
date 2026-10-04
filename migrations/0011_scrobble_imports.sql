-- =============================================================
--  Scrobblr — Listening-history imports (Last.fm)
--
--  One row per import job. The worker walks the provider's history in
--  pages; each page commits together with the cursor below, so a crash
--  or restart resumes where the last committed page left off.
-- =============================================================

CREATE TABLE scrobble_imports (
    id                BIGSERIAL    PRIMARY KEY,
    user_id           BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider          TEXT         NOT NULL CHECK (provider IN ('lastfm')),
    external_user     TEXT         NOT NULL,
    -- Ownership proven through the provider's auth; false for imports an
    -- operator started from the CLI.
    verified          BOOLEAN      NOT NULL,
    status            TEXT         NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'running', 'done', 'failed', 'cancelled')),
    error_code        TEXT,        -- user_not_found | history_hidden | cap_reached | lastfm_unavailable | lastfm_error
    error_message     TEXT,
    -- Scrobbles in (window_from, window_to]; window_to is fixed on the first run.
    window_from       TIMESTAMPTZ,
    window_to         TIMESTAMPTZ,
    -- Resume cursor (shared::lastfm::Cursor).
    segment_to        BIGINT,
    segment_page      INT,
    segment_oldest    BIGINT,
    total_expected    BIGINT,
    fetched           BIGINT       NOT NULL DEFAULT 0,
    imported          BIGINT       NOT NULL DEFAULT 0,
    duplicates        BIGINT       NOT NULL DEFAULT 0,
    skipped           BIGINT       NOT NULL DEFAULT 0,
    oldest_played_at  TIMESTAMPTZ,
    -- Imported but not yet handed to the aggregates, classifier and
    -- enrichment (done at checkpoints, not per scrobble).
    pending_from      TIMESTAMPTZ,
    pending_to        TIMESTAMPTZ,
    attempts          INT          NOT NULL DEFAULT 0,   -- consecutive transient failures
    next_attempt_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    lease_token       UUID,
    leased_until      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    started_at        TIMESTAMPTZ,
    finished_at       TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX uq_scrobble_imports_active ON scrobble_imports (user_id)
    WHERE status IN ('pending', 'running');
CREATE INDEX idx_scrobble_imports_user ON scrobble_imports (user_id, created_at DESC);
CREATE INDEX idx_scrobble_imports_due ON scrobble_imports (next_attempt_at)
    WHERE status IN ('pending', 'running');

CREATE TRIGGER trg_scrobble_imports_updated_at
    BEFORE UPDATE ON scrobble_imports
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Set only by imports, so a client can't pass live scrobbles off as
-- imported (scrobbles.source is client-supplied).
ALTER TABLE scrobbles ADD COLUMN import_id BIGINT;

-- Imports insert page-sized batches and update these counters set-based:
-- per row, the same artist and user rows would be updated hundreds of times
-- in one transaction, and last_seen_at would move back to the oldest play.
CREATE OR REPLACE FUNCTION increment_scrobble_counts()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.import_id IS NOT NULL THEN
        RETURN NEW;
    END IF;

    UPDATE tracks  SET scrobble_count = scrobble_count + 1 WHERE id = NEW.track_id;
    UPDATE artists SET scrobble_count = scrobble_count + 1 WHERE id = NEW.artist_id;
    UPDATE users   SET scrobble_count = scrobble_count + 1,
                       last_seen_at   = NEW.played_at
    WHERE id = NEW.user_id;

    IF NEW.album_id IS NOT NULL THEN
        UPDATE albums SET scrobble_count = scrobble_count + 1 WHERE id = NEW.album_id;
    END IF;

    RETURN NEW;
END;
$$;

-- MusicBrainz recording id as Last.fm reported it. Unverified, so it is a
-- hint the enrichment job checks, never written to tracks.mbid as is.
ALTER TABLE tracks ADD COLUMN mbid_hint UUID;

-- When Last.fm was last asked for the track's length (duration backfill).
ALTER TABLE tracks ADD COLUMN lastfm_checked_at TIMESTAMPTZ;

CREATE INDEX idx_tracks_lastfm_duration_backlog ON tracks (scrobble_count DESC, id)
    WHERE duration_ms IS NULL AND mb_duration_ms IS NULL AND lastfm_checked_at IS NULL;
