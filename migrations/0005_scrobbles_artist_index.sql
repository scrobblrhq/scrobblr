-- =============================================================
--  newfm — Global artist scrobble index
--  The artist social sections (artist_top_tracks, artist_listeners)
--  filter scrobbles by artist_id across all users; the existing indexes
--  are all user_id- or track_id-leading, so those queries had no usable
--  index. Mirrors idx_scrobbles_track_global for artist_id.
-- =============================================================

CREATE INDEX idx_scrobbles_artist_global ON scrobbles (artist_id, played_at DESC);
