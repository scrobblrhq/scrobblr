-- =============================================================
--  Scrobblr — Track lengths from Deezer
--
--  Tracks no source has a length for leave their scrobbles no_data. The
--  worker asks Deezer's public search about them, and about tracks whose
--  catalog and MusicBrainz lengths disagree, where a third opinion tells
--  which is wrong (worker tracks mb-review). Deezer's length is kept apart
--  so it stays an independent witness; the classifier and the ranking
--  weights use it as the catalog length when the catalog has none.
-- =============================================================

ALTER TABLE tracks ADD COLUMN deezer_duration_ms INT;
-- When Deezer was last asked; misses are asked again after 30 days.
ALTER TABLE tracks ADD COLUMN deezer_checked_at TIMESTAMPTZ;

CREATE INDEX idx_tracks_deezer_no_length ON tracks (scrobble_count DESC, id)
    WHERE deezer_checked_at IS NULL AND duration_ms IS NULL AND mb_duration_ms IS NULL;

CREATE INDEX idx_tracks_deezer_disputed ON tracks (scrobble_count DESC, id)
    WHERE deezer_checked_at IS NULL AND duration_ms > 0 AND mb_duration_ms > 0
      AND GREATEST(duration_ms, mb_duration_ms) * 2 >= LEAST(duration_ms, mb_duration_ms) * 3;
