-- =============================================================
--  Scrobblr — Full Schema
--  PostgreSQL + TimescaleDB
-- =============================================================

-- =============================================================
--  EXTENSIONS
-- =============================================================

CREATE EXTENSION IF NOT EXISTS timescaledb;
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS pg_trgm;      -- Fuzzy search for artists/tracks
CREATE EXTENSION IF NOT EXISTS unaccent;      -- Name normalization


-- =============================================================
--  USERS
-- =============================================================

CREATE TABLE users (
                       id                BIGSERIAL       PRIMARY KEY,
                       username          TEXT            NOT NULL UNIQUE,
                       email             TEXT            NOT NULL UNIQUE,
                       password_hash     TEXT            NOT NULL,
                       display_name      TEXT,
                       bio               TEXT,
                       image_url         TEXT,
                       website_url       TEXT,
                       country           CHAR(2),                        -- ISO 3166-1 alpha-2
                       scrobble_count    BIGINT          NOT NULL DEFAULT 0,
                       is_private        BOOLEAN         NOT NULL DEFAULT FALSE,
                       is_verified       BOOLEAN         NOT NULL DEFAULT FALSE,
                       last_seen_at      TIMESTAMPTZ,
                       created_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                       updated_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_users_username     ON users (username);
CREATE INDEX idx_users_email        ON users (email);
CREATE INDEX idx_users_last_seen    ON users (last_seen_at DESC);


-- =============================================================
--  MUSIC CATALOG
-- =============================================================

CREATE TABLE artists (
                         id                BIGSERIAL       PRIMARY KEY,
                         name              TEXT            NOT NULL,
                         name_normalized   TEXT            NOT NULL,       -- lower + unaccent, for deduplication
                         mbid              UUID            UNIQUE,         -- MusicBrainz ID
                         image_url         TEXT,
                         bio               TEXT,
                         scrobble_count    BIGINT          NOT NULL DEFAULT 0,
                         listener_count    BIGINT          NOT NULL DEFAULT 0,
                         created_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

                         UNIQUE (name_normalized)
);

CREATE INDEX idx_artists_name_trgm  ON artists USING gin (name gin_trgm_ops);
CREATE INDEX idx_artists_scrobbles  ON artists (scrobble_count DESC);


CREATE TABLE albums (
                        id                BIGSERIAL       PRIMARY KEY,
                        artist_id         BIGINT          NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
                        title             TEXT            NOT NULL,
                        title_normalized  TEXT            NOT NULL,
                        mbid              UUID            UNIQUE,
                        image_url         TEXT,
                        release_date      DATE,
                        scrobble_count    BIGINT          NOT NULL DEFAULT 0,
                        created_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

                        UNIQUE (artist_id, title_normalized)
);

CREATE INDEX idx_albums_artist      ON albums (artist_id);
CREATE INDEX idx_albums_scrobbles   ON albums (scrobble_count DESC);


CREATE TABLE tracks (
                        id                BIGSERIAL       PRIMARY KEY,
                        artist_id         BIGINT          NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
                        album_id          BIGINT          REFERENCES albums (id) ON DELETE SET NULL,
                        title             TEXT            NOT NULL,
                        title_normalized  TEXT            NOT NULL,
                        mbid              UUID            UNIQUE,
                        duration_ms       INT,                            -- Duration in milliseconds
                        scrobble_count    BIGINT          NOT NULL DEFAULT 0,
                        created_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

                        UNIQUE (artist_id, title_normalized)
);

CREATE INDEX idx_tracks_artist      ON tracks (artist_id);
CREATE INDEX idx_tracks_album       ON tracks (album_id);
CREATE INDEX idx_tracks_scrobbles   ON tracks (scrobble_count DESC);
CREATE INDEX idx_tracks_title_trgm  ON tracks USING gin (title gin_trgm_ops);


-- =============================================================
--  SCROBBLES  (hypertable — the heart of the system)
-- =============================================================

CREATE TABLE scrobbles (
                           id                BIGSERIAL,
                           user_id           BIGINT          NOT NULL REFERENCES users (id)   ON DELETE CASCADE,
                           track_id          BIGINT          NOT NULL REFERENCES tracks (id)  ON DELETE CASCADE,

    -- Denormalized to avoid joins in analytical queries
                           artist_id         BIGINT          NOT NULL,
                           album_id          BIGINT,

                           played_at         TIMESTAMPTZ     NOT NULL,       -- TimescaleDB partition key
                           source            TEXT            NOT NULL DEFAULT 'extension',
    -- 'extension' | 'spotify' | 'manual' | 'import'
                           duration_ms       INT,                            -- How long they actually listened (can be < track.duration_ms)

                           PRIMARY KEY (id, played_at)                       -- Composite PK required by TimescaleDB
);

-- Convert to hypertable, partitioning by week
SELECT create_hypertable('scrobbles', 'played_at', chunk_time_interval => INTERVAL '7 days');

-- Critical indexes for the most frequent queries
CREATE INDEX idx_scrobbles_user_time    ON scrobbles (user_id, played_at DESC);
CREATE INDEX idx_scrobbles_user_artist  ON scrobbles (user_id, artist_id, played_at DESC);
CREATE INDEX idx_scrobbles_user_track   ON scrobbles (user_id, track_id,  played_at DESC);
CREATE INDEX idx_scrobbles_track_global ON scrobbles (track_id, played_at DESC);

-- Automatic compression for chunks > 30 days
ALTER TABLE scrobbles SET (
    timescaledb.compress,
    timescaledb.compress_orderby = 'played_at DESC',
    timescaledb.compress_segmentby = 'user_id'
    );

SELECT add_compression_policy('scrobbles', INTERVAL '30 days');

-- Retention (optional, adjustable): delete data > 10 years
-- SELECT add_retention_policy('scrobbles', INTERVAL '10 years');


-- =============================================================
--  NOW PLAYING  (ephemeral state, one row per user)
-- =============================================================

CREATE TABLE now_playing (
                             user_id          BIGINT          PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
                             track_id         BIGINT          NOT NULL REFERENCES tracks (id),
                             artist_id        BIGINT          NOT NULL,
                             album_id         BIGINT,
                             started_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                             expires_at       TIMESTAMPTZ     NOT NULL,       -- NOW() + track.duration_ms; if expired → no longer listening
                             source           TEXT            NOT NULL DEFAULT 'extension'
);

CREATE INDEX idx_now_playing_expires ON now_playing (expires_at);


-- =============================================================
--  SOCIAL
-- =============================================================

CREATE TABLE user_follows (
                              follower_id      BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
                              followee_id      BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
                              created_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

                              PRIMARY KEY (follower_id, followee_id),
                              CHECK (follower_id <> followee_id)
);

CREATE INDEX idx_follows_followee   ON user_follows (followee_id);


-- =============================================================
--  AUTHENTICATION
-- =============================================================

CREATE TABLE user_sessions (
                               id               UUID            PRIMARY KEY DEFAULT uuid_generate_v4(),
                               user_id          BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
                               ip_address       INET,
                               user_agent       TEXT,
                               created_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                               expires_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW() + INTERVAL '30 days',
                               last_used_at     TIMESTAMPTZ     NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_sessions_user      ON user_sessions (user_id);
CREATE INDEX idx_sessions_expires   ON user_sessions (expires_at);


CREATE TABLE api_tokens (
                            id               UUID            PRIMARY KEY DEFAULT uuid_generate_v4(),
                            user_id          BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
                            name             TEXT            NOT NULL,       -- "My Chrome Extension", "Personal Script"
                            token_hash       TEXT            NOT NULL UNIQUE,
                            scopes           TEXT[]          NOT NULL DEFAULT '{scrobble}',
    -- 'scrobble' | 'read' | 'write'
                            last_used_at     TIMESTAMPTZ,
                            created_at       TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
                            expires_at       TIMESTAMPTZ                     -- NULL = does not expire
);

CREATE INDEX idx_tokens_user        ON api_tokens (user_id);
CREATE INDEX idx_tokens_hash        ON api_tokens (token_hash);


-- =============================================================
--  CONTINUOUS AGGREGATES  (Timescale materialized views)
--  Automatically updated in the background, zero query performance cost.
-- =============================================================

-- Daily rollup by user + artist
-- Powers: top artists of the month, year, all-time
CREATE MATERIALIZED VIEW scrobbles_daily_by_artist
            WITH (timescaledb.continuous) AS
SELECT
    user_id,
    artist_id,
    time_bucket('1 day', played_at)  AS day,
    COUNT(*)                         AS play_count
FROM scrobbles
GROUP BY user_id, artist_id, day
WITH NO DATA;

SELECT add_continuous_aggregate_policy('scrobbles_daily_by_artist',
                                       start_offset      => INTERVAL '3 days',
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );


-- Daily rollup by user + track
-- Powers: top tracks of the period
CREATE MATERIALIZED VIEW scrobbles_daily_by_track
            WITH (timescaledb.continuous) AS
SELECT
    user_id,
    track_id,
    artist_id,
    time_bucket('1 day', played_at)  AS day,
    COUNT(*)                         AS play_count
FROM scrobbles
GROUP BY user_id, track_id, artist_id, day
WITH NO DATA;

SELECT add_continuous_aggregate_policy('scrobbles_daily_by_track',
                                       start_offset      => INTERVAL '3 days',
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );


-- Global daily activity of the user (for profile heatmap)
CREATE MATERIALIZED VIEW user_activity_daily
            WITH (timescaledb.continuous) AS
SELECT
    user_id,
    time_bucket('1 day', played_at)  AS day,
    COUNT(*)                         AS scrobble_count
FROM scrobbles
GROUP BY user_id, day
WITH NO DATA;

SELECT add_continuous_aggregate_policy('user_activity_daily',
                                       start_offset      => INTERVAL '3 days',
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );


-- =============================================================
--  EXAMPLE QUERIES using aggregates
-- =============================================================

-- Top 10 artists of the last month (instantaneous, reads from aggregate)
--
--   SELECT a.name, SUM(s.play_count) AS plays
--   FROM scrobbles_daily_by_artist s
--   JOIN artists a ON a.id = s.artist_id
--   WHERE s.user_id = $1
--     AND s.day >= NOW() - INTERVAL '30 days'
--   GROUP BY a.name
--   ORDER BY plays DESC
--   LIMIT 10;
--
-- Activity heatmap for the year
--
--   SELECT day, scrobble_count
--   FROM user_activity_daily
--   WHERE user_id = $1
--     AND day >= NOW() - INTERVAL '1 year'
--   ORDER BY day;


-- =============================================================
--  TRIGGERS — keeps counters and updated_at synchronized
-- =============================================================

-- Automatic updated_at
CREATE OR REPLACE FUNCTION set_updated_at()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_users_updated_at
    BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();


-- Increments scrobble_count across tracks, artists, albums, and users
-- when inserting a new scrobble
CREATE OR REPLACE FUNCTION increment_scrobble_counts()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
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

CREATE TRIGGER trg_scrobble_counts
    AFTER INSERT ON scrobbles
    FOR EACH ROW EXECUTE FUNCTION increment_scrobble_counts();

