-- =============================================================
--  Scrobblr — Albums decided by accounts
--
--  A track belonged to the first album a scrobble named, so one account
--  could file a popular track under any album for everyone. Each account
--  now has one say per track (track_album_votes): the album its plays of
--  it named, the latest when they differ. The worker decides each queued
--  track's album from them (shared::albums, docs/social.md): accounts old
--  enough to count, flagged ones at their coordination factor, a release
--  MusicBrainz lists the recording on counting double, and a new album
--  has to beat the current one by half again. tracks.album_id stays what
--  every query reads; the first album named holds it until then.
--  `worker albums backfill` makes the votes of the history so far.
-- =============================================================

CREATE TABLE track_album_votes (
    track_id   BIGINT       NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    user_id    BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    album_id   BIGINT       NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    played_at  TIMESTAMPTZ  NOT NULL,

    PRIMARY KEY (track_id, user_id)
);

-- For erasing an account. Albums go only with their artist, whose tracks'
-- votes go with the tracks, so album_id has none.
CREATE INDEX idx_track_album_votes_user ON track_album_votes (user_id);

CREATE TABLE album_attribution_queue (
    track_id  BIGINT       PRIMARY KEY REFERENCES tracks (id) ON DELETE CASCADE,
    due_at    TIMESTAMPTZ  NOT NULL
);

CREATE INDEX idx_album_attribution_queue_due ON album_attribution_queue (due_at);

-- The titles (lowercased, as albums.title_normalized) of the releases
-- MusicBrainz lists the track's recording on, from its last lookup; NULL
-- until one.
ALTER TABLE tracks ADD COLUMN mb_release_titles TEXT[];
