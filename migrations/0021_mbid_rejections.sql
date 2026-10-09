-- =============================================================
--  Scrobblr — MusicBrainz matches taken back
--
--  A track's mbid is never overwritten, but a match can be wrong (a
--  snippet, a live take, a medley). `worker tracks mb-review --apply`
--  unlinks the matches whose length two other sources contradict and
--  records them here, so enrichment never adopts that recording for the
--  track again.
-- =============================================================

CREATE TABLE track_mbid_rejections (
    track_id             BIGINT       NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    mbid                 UUID         NOT NULL,
    -- The lengths that decided it.
    mb_duration_ms       INT,
    catalog_duration_ms  INT,
    deezer_duration_ms   INT,
    rejected_at          TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (track_id, mbid)
);
