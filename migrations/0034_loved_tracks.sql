-- =============================================================
--  Scrobblr — Loved tracks
--
--  A user's loved tracks (docs/social.md, Loved tracks). Loves follow the
--  profile's visibility and feed nothing else: no ranking, recommendation,
--  similarity or album decision reads them. `source` is the path a love
--  came by; `client_id` the Last.fm-compatible client that sent it,
--  `import_id` the import that brought it (set by the server, no foreign
--  key, like `scrobbles.import_id`).
-- =============================================================

CREATE TABLE loved_tracks (
    user_id    BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    track_id   BIGINT       NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    loved_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    source     TEXT         NOT NULL
               CHECK (source IN ('scrobblr', 'lastfm', 'audioscrobbler', 'lastfm_import')),
    client_id  INT          REFERENCES scrobble_clients (id),
    import_id  BIGINT,

    PRIMARY KEY (user_id, track_id)
);

-- A user's list, newest first, paged by (loved_at, track_id).
CREATE INDEX idx_loved_tracks_user_recent ON loved_tracks (user_id, loved_at DESC, track_id DESC);
-- The cascade when a track leaves the catalog.
CREATE INDEX idx_loved_tracks_track ON loved_tracks (track_id);
