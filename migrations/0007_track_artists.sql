-- =============================================================
--  Scrobblr — Multiple artists per track
--
--  Chosen over a `featured_artist_ids BIGINT[]` column on `tracks`:
--  Postgres cannot put a foreign key on array elements, so an array would
--  leave dangling artist ids behind every artist merge or delete, and it
--  could not carry a per-credit role. This table gets real referential
--  integrity via ON DELETE CASCADE plus an index in the artist -> track
--  direction, which an array can only approximate with a GIN index.
--
--  `tracks.artist_id` stays the denormalized primary artist: it backs the
--  UNIQUE (artist_id, title_normalized) identity of a track and the
--  `scrobbles.artist_id` used by every aggregate query. This table is the
--  full credit list, and the invariant is that the 'primary' row always
--  mirrors `tracks.artist_id`.
-- =============================================================

CREATE TYPE track_artist_role AS ENUM ('primary', 'featured');

CREATE TABLE track_artists (
    track_id    BIGINT             NOT NULL REFERENCES tracks (id)  ON DELETE CASCADE,
    artist_id   BIGINT             NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    role        track_artist_role  NOT NULL DEFAULT 'featured',
    -- Billing order within the role, so "A feat. B, C" round-trips.
    position    INT                NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ        NOT NULL DEFAULT NOW(),

    PRIMARY KEY (track_id, artist_id)
);

CREATE INDEX idx_track_artists_artist ON track_artists (artist_id);

CREATE UNIQUE INDEX uq_track_artists_primary ON track_artists (track_id)
    WHERE role = 'primary';

INSERT INTO track_artists (track_id, artist_id, role, position)
SELECT id, artist_id, 'primary', 0
FROM tracks
ON CONFLICT (track_id, artist_id) DO NOTHING;
