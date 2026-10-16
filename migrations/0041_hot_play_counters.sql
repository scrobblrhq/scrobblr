-- Every scrobble adds one to its track's, artist's and album's
-- scrobble_count and moves its user's last_seen_at. With either column in
-- an index, each of those updates wrote a new entry into every index of
-- the table (eleven for tracks) and bloated them all. Out of every index,
-- the updates can stay on their page (HOT) when it has room, which
-- fillfactor leaves.

-- Nothing reads these in order: the backlogs below sort their few rows,
-- the re-sweeps (every 6 hours) and the enrichment backfill read their own
-- small sets, and community charts and the feed find users by id.
DROP INDEX idx_tracks_scrobbles;
DROP INDEX idx_artists_scrobbles;
DROP INDEX idx_albums_scrobbles;
DROP INDEX idx_users_last_seen;

DROP INDEX idx_tracks_lastfm_duration_backlog;
CREATE INDEX idx_tracks_lastfm_duration_backlog ON tracks (id)
    WHERE duration_ms IS NULL AND mb_duration_ms IS NULL AND lastfm_checked_at IS NULL;

DROP INDEX idx_tracks_deezer_no_length;
CREATE INDEX idx_tracks_deezer_no_length ON tracks (id)
    WHERE deezer_checked_at IS NULL AND duration_ms IS NULL AND mb_duration_ms IS NULL;

DROP INDEX idx_tracks_deezer_disputed;
CREATE INDEX idx_tracks_deezer_disputed ON tracks (id)
    WHERE deezer_checked_at IS NULL AND duration_ms > 0 AND mb_duration_ms > 0
      AND GREATEST(duration_ms, mb_duration_ms) * 2 >= LEAST(duration_ms, mb_duration_ms) * 3;

-- New pages only: rows move to pages with room as they are updated.
ALTER TABLE tracks SET (fillfactor = 90);
ALTER TABLE artists SET (fillfactor = 90);
ALTER TABLE albums SET (fillfactor = 90);
ALTER TABLE users SET (fillfactor = 90);
