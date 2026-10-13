-- One heavy listener with three years of history, on top of the synthetic
-- population (population.sql), for measuring a personal chart over all
-- of it (docs/social.md):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/heavy.sql
--   DATABASE_URL="$SCRATCH_URL" worker classify reclassify --user heavy
--
-- Deterministic. `heavy` played on 9 days in 10 from 1,095 to 61 days ago
-- (before the population's window), 80 to 220 plays a day back to back,
-- 70 % from 300 favourite artists, 4 % reported again seconds later:
-- 146,691 scrobbles. They are stored as an import (the per-row counter
-- trigger skips them; the aggregates' invalidations are kept), so they
-- weigh nothing in rankings; the compression policy compresses them with
-- the other old chunks.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE username = 'synth1') THEN
        RAISE EXCEPTION 'run population.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM users WHERE username = 'heavy') THEN
        RAISE EXCEPTION 'heavy is there already';
    END IF;
END $$;

SELECT setseed(0.29);

BEGIN;

INSERT INTO users (username, email, password_hash, created_at)
VALUES ('heavy', 'heavy@synthetic.invalid', 'x', NOW() - INTERVAL '1100 days');

CREATE TEMP TABLE heavy_favourites AS
SELECT n AS slot, a.id AS artist_id
FROM generate_series(1, 300) n
CROSS JOIN LATERAL (SELECT id FROM artists ORDER BY random() + n * 0 LIMIT 1) a;

CREATE TEMP TABLE heavy_tracks AS
SELECT t.artist_id, row_number() OVER (PARTITION BY t.artist_id ORDER BY t.id) AS rank,
       t.id, COALESCE(t.duration_ms, 200000) AS length
FROM tracks t;
CREATE INDEX ON heavy_tracks (artist_id, rank);

CREATE TEMP TABLE heavy_artists AS
SELECT row_number() OVER (ORDER BY id) AS rank, id FROM artists;
CREATE INDEX ON heavy_artists (rank);

-- Draws in select lists, so random() runs per play.
CREATE TEMP TABLE heavy_plays AS
WITH days AS (
    SELECT * FROM (
        SELECT d::date AS day, random() AS draw, 80 + floor(random() * 140)::int AS plays
        FROM generate_series(CURRENT_DATE - 1095, CURRENT_DATE - 61, INTERVAL '1 day') d
    ) drawn
    WHERE draw < 0.9
), slots AS (
    SELECT days.day, seq, random() < 0.7 AS favourite,
           1 + floor(random() * 300)::int AS slot,
           1 + floor(random() * (SELECT count(*) FROM heavy_artists))::int AS any_rank,
           1 + LEAST(7, floor(power(8, random()))::int - 1) AS track_rank
    FROM days
    CROSS JOIN LATERAL generate_series(1, days.plays) seq
), picked AS (
    SELECT s.day, s.seq, s.track_rank,
           CASE WHEN s.favourite THEN f.artist_id ELSE a.id END AS artist_id
    FROM slots s
    JOIN heavy_favourites f ON f.slot = s.slot
    JOIN heavy_artists a ON a.rank = s.any_rank
)
SELECT p.day, p.seq, t.id AS track_id, t.artist_id, t.length,
       (p.day + make_interval(secs => 21600 + (sum(t.length) OVER w - t.length) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
JOIN heavy_tracks t ON t.artist_id = p.artist_id AND t.rank = p.track_rank
WINDOW w AS (PARTITION BY p.day ORDER BY p.seq);

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, import_id)
SELECT u.id, p.track_id, p.artist_id, p.played_at, 'lastfm_import', p.length, NULL, 0
FROM heavy_plays p
CROSS JOIN (SELECT id FROM users WHERE username = 'heavy') u
ORDER BY p.played_at;

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, import_id)
SELECT s.user_id, s.track_id, s.artist_id, s.played_at + make_interval(secs => 3 + floor(random() * 7)),
       s.source, s.duration_ms, NULL, 0
FROM scrobbles s
WHERE s.user_id = (SELECT id FROM users WHERE username = 'heavy') AND random() < 0.04
ORDER BY s.played_at;

UPDATE users u SET scrobble_count = c.n, last_seen_at = c.last
FROM (SELECT user_id, count(*) AS n, max(played_at) AS last FROM scrobbles
      WHERE user_id = (SELECT id FROM users WHERE username = 'heavy') GROUP BY user_id) c
WHERE c.user_id = u.id;

COMMIT;

CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', CURRENT_DATE - 1096, CURRENT_DATE - 60);
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  CURRENT_DATE - 1096, CURRENT_DATE - 60);
CALL refresh_continuous_aggregate('user_activity_daily',       CURRENT_DATE - 1096, CURRENT_DATE - 60);

SELECT count(*) AS scrobbles, min(played_at), max(played_at)
FROM scrobbles WHERE user_id = (SELECT id FROM users WHERE username = 'heavy');
