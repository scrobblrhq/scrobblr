-- =============================================================
--  Scrobblr — Artist and track sections read the daily aggregates
--
--  artist_top_tracks, artist_listeners and track_listeners counted raw
--  scrobbles across all users. Compressed chunks are segmented by user,
--  so those public queries decompressed every chunk of history; they now
--  sum the daily aggregates, and these covering indexes let them read
--  only the entity's rows (index-only).
-- =============================================================

CREATE INDEX idx_daily_by_artist_artist ON scrobbles_daily_by_artist (artist_id)
    INCLUDE (user_id, play_count);
CREATE INDEX idx_daily_by_track_track   ON scrobbles_daily_by_track (track_id)
    INCLUDE (user_id, play_count);
CREATE INDEX idx_daily_by_track_artist  ON scrobbles_daily_by_track (artist_id)
    INCLUDE (track_id, play_count);
