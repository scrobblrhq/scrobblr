-- Synthetic listening population, for measuring rankings and the
-- anti-botting pipeline at scale. Run against a scratch database migrated
-- with `worker migrate`, never a real one (it refuses one with users):
--
--   psql "$SCRATCH_URL" -v users=4000 -v days=60 -f scripts/synthetic/population.sql
--
-- Run it as a superuser (it skips triggers). Deterministic for given
-- variables. Every user is established (registered 60 to 900 days ago,
-- evenly), listens in sessions of back-to-back plays (so the
-- classifier counts them), mostly to 25 favourite artists drawn from a
-- Zipf-like popularity, through one client each: the extension or the
-- mobile app (listened time at the scrobble point), the Spotify poller,
-- a ListenBrainz scrobbler without listened time, or an unknown script.
-- 4 % of plays are reported again a few seconds later. Afterwards:
-- refresh the aggregates (end of this file), then classify and weigh
-- (docs/rankings.md).
--
-- With -v genres=N (0, the default, makes the population above, the same
-- as before the option), tastes form communities: each artist has one of
-- N genres, each user a main genre and maybe a second and a third, and
-- favourites are drawn by genre (70 %, 20 %, 10 %), Zipf-like within it.
-- Users discover them over time: 15 favourites from the first day, one
-- more about every 6th day of 60, so recent listening holds artists new
-- to the user, of their genres. synthetic_genres and
-- synthetic_artist_genres keep who and what is in which
-- (docs/social.md, Recommendations).

\set ON_ERROR_STOP on
\if :{?users}
\else
  \set users 4000
\endif
\if :{?days}
\else
  \set days 60
\endif
\if :{?artists}
\else
  \set artists 20000
\endif
\if :{?genres}
\else
  \set genres 0
\endif
SELECT :genres > 0 AS taste \gset

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM users) THEN
        RAISE EXCEPTION 'refusing to add a synthetic population to a database with users';
    END IF;
END $$;

SELECT setseed(0.42);

BEGIN;

-- The per-row counter trigger fires on chunks whatever ALTER TABLE says
-- (counters are set below, in bulk); skipping triggers needs a superuser.
-- The aggregates' first refresh below covers everything without them.
SET LOCAL session_replication_role = replica;

INSERT INTO scrobble_clients (protocol, name, verified) VALUES
    ('scrobblr',     'ytmusic',            FALSE),
    ('scrobblr',     'android',            FALSE),
    ('spotify',      'spotify',            TRUE),
    ('listenbrainz', 'Pano Scrobbler 3.12', FALSE),
    ('scrobblr',     'myscript',           FALSE);

CREATE TEMP TABLE synth_clients AS
SELECT kind, id FROM (VALUES
    ('extension', 'scrobblr', 'ytmusic'),
    ('mobile',    'scrobblr', 'android'),
    ('spotify',   'spotify',  'spotify'),
    ('compat',    'listenbrainz', 'Pano Scrobbler 3.12'),
    ('script',    'scrobblr', 'myscript')) k(kind, protocol, name)
JOIN scrobble_clients c USING (protocol, name);

-- Catalog: artists ranked by popularity, 8 tracks each.
INSERT INTO artists (name, name_normalized, enriched_at)
SELECT 'Synth Artist ' || n, 'synth artist ' || n, NOW()
FROM generate_series(1, :artists) n;

CREATE TEMP TABLE synth_artists AS
SELECT row_number() OVER (ORDER BY id) AS rank, id FROM artists;
CREATE INDEX ON synth_artists (rank);

-- Lengths around 3.7 minutes; 3 % of tracks have none anywhere, the rest
-- a catalog length and 80 % a MusicBrainz one.
INSERT INTO tracks (artist_id, title, title_normalized, duration_ms, mb_duration_ms,
                    enriched_at, lastfm_checked_at)
SELECT artist_id, title, lower(title),
       CASE WHEN known THEN length END,
       CASE WHEN known AND mb THEN length END,
       NOW(), NOW()
FROM (
    SELECT a.id AS artist_id, 'Track ' || n AS title,
           (120000 + floor(random() * 120000) + floor(random() * 120000))::int AS length,
           random() >= 0.03 AS known,
           random() < 0.8 AS mb
    FROM synth_artists a
    CROSS JOIN generate_series(1, 8) n
) t;

INSERT INTO track_artists (track_id, artist_id, role, position)
SELECT id, artist_id, 'primary', 0 FROM tracks;

CREATE TEMP TABLE synth_tracks AS
SELECT t.artist_id, row_number() OVER (PARTITION BY t.artist_id ORDER BY t.id) AS rank,
       t.id, COALESCE(t.duration_ms, (120000 + (t.id * 7919) % 240000)::int) AS length,
       t.duration_ms IS NOT NULL AS known
FROM tracks t;
CREATE INDEX ON synth_tracks (artist_id, rank);

-- Users: how often they listen, and through what.
-- Registration days come from the user's number, not random(), so the
-- draws below are the same as before they were spread.
INSERT INTO users (username, email, password_hash, created_at)
SELECT 'synth' || n, 'synth' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 7919) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :users) n;

CREATE TEMP TABLE synth_users AS
SELECT u.id,
       0.15 + random() * 0.75 AS activity,
       (ARRAY['extension','extension','extension','extension','mobile','mobile','spotify','compat','compat','compat','compat','compat','script'])
           [1 + floor(random() * 13)::int] AS kind
FROM users u;

\if :taste
-- An artist's genre comes from its rank; within a genre, artists keep the
-- order of their global ranks, so each genre has its heads and its tail.
CREATE TABLE synthetic_artist_genres AS
SELECT a.id AS artist_id, a.rank, (hashint4(a.rank::int) & 2147483647) % :genres AS genre
FROM synth_artists a;
CREATE TEMP TABLE synth_genre_artists AS
SELECT genre, rank, row_number() OVER (PARTITION BY genre ORDER BY rank) AS genre_rank
FROM synthetic_artist_genres;
CREATE INDEX ON synth_genre_artists (genre, genre_rank);
CREATE TEMP TABLE synth_genre_sizes AS
SELECT genre, count(*) AS size FROM synthetic_artist_genres GROUP BY genre;

CREATE TABLE synthetic_genres AS
SELECT id AS user_id, floor(random() * :genres)::int AS g1,
       CASE WHEN random() < 0.5 THEN floor(random() * :genres)::int END AS g2,
       CASE WHEN random() < 0.2 THEN floor(random() * :genres)::int END AS g3
FROM synth_users;

-- Each user's 25 favourites: a genre by the user's weights, then an
-- artist of it, Zipf-like within the genre. Draws in a subquery, so
-- random() runs per slot.
CREATE TEMP TABLE synth_favourites AS
SELECT d.user_id, d.slot, ga.rank AS artist_rank
FROM (
    SELECT g.user_id, s.slot,
           CASE WHEN r < 0.7 THEN g.g1
                WHEN r < 0.9 THEN COALESCE(g.g2, g.g1)
                ELSE COALESCE(g.g3, g.g2, g.g1) END AS genre,
           u
    FROM synthetic_genres g
    CROSS JOIN generate_series(1, 25) s(slot)
    CROSS JOIN LATERAL (SELECT random() + s.slot * 0 AS r, random() + s.slot * 0 AS u) draws
) d
JOIN synth_genre_sizes z ON z.genre = d.genre
JOIN synth_genre_artists ga
  ON ga.genre = d.genre AND ga.genre_rank = LEAST(z.size, floor(power(z.size, d.u)))::int;
\else
-- Each user's 25 favourite artists, Zipf-like: rank = floor(N ^ u).
CREATE TEMP TABLE synth_favourites AS
SELECT u.id AS user_id, s.slot,
       LEAST(:artists, floor(power(:artists, random()))::int) AS artist_rank
FROM synth_users u
CROSS JOIN generate_series(1, 25) s(slot);
\endif
CREATE INDEX ON synth_favourites (user_id, slot);

-- Sessions: one per active day, 8 to 52 plays back to back from a random
-- hour; 85 % of plays from favourites, the rest from the whole catalog.
CREATE TEMP TABLE synth_plays AS
WITH user_days AS (
    -- The draw is made in a subquery: as a WHERE clause it would be pushed
    -- down to the users scan and made once per user, not per day.
    SELECT * FROM (
        SELECT u.id AS user_id, u.kind, d::date AS day, u.activity, random() AS draw,
               8 + floor(random() * 45)::int AS plays,
               (6 + floor(random() * 12)) * 3600 + floor(random() * 3600) AS start_secs
        FROM synth_users u
        CROSS JOIN generate_series(CURRENT_DATE - :days, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE draw < activity
), slots AS (
    SELECT ud.*, seq,
           CASE WHEN random() < 0.85
                THEN NULL
                ELSE LEAST(:artists, floor(power(:artists, random()))::int) END AS global_rank,
           1 + floor(random() * (CASE WHEN :'taste'::boolean
                                      THEN LEAST(25, 15 + floor(11.0 * (ud.day - (CURRENT_DATE - :days)) / :days))
                                      ELSE 25 END)::float8)::int AS slot,
           1 + LEAST(7, floor(power(8, random()))::int - 1) AS track_rank,
           floor(random() * 8000)::int AS gap_ms
    FROM user_days ud
    CROSS JOIN LATERAL generate_series(1, ud.plays) seq
), picked AS (
    SELECT s.user_id, s.kind, s.day, s.start_secs, s.seq, s.gap_ms, t.id AS track_id,
           t.artist_id, t.length, t.known
    FROM slots s
    JOIN synth_favourites f ON f.user_id = s.user_id AND f.slot = s.slot
    JOIN synth_artists a ON a.rank = COALESCE(s.global_rank, f.artist_rank)
    JOIN synth_tracks t ON t.artist_id = a.id AND t.rank = s.track_rank
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + p.gap_ms) OVER w - p.length - p.gap_ms) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT p.user_id, p.track_id, p.artist_id, p.played_at,
       CASE p.kind WHEN 'extension' THEN 'ytmusic' WHEN 'mobile' THEN 'android'
                   WHEN 'spotify' THEN 'spotify' WHEN 'compat' THEN 'listenbrainz'
                   ELSE 'myscript' END,
       CASE WHEN p.known THEN p.length END,
       CASE p.kind
           WHEN 'extension' THEN LEAST(p.length / 2, 240000) + floor(random() * 2000)::int
           WHEN 'mobile'    THEN GREATEST(30000, LEAST(p.length / 2, 240000)) + floor(random() * 500)::int
           WHEN 'spotify'   THEN p.length
           WHEN 'script'    THEN p.length
       END,
       c.id
FROM synth_plays p
JOIN synth_clients c ON c.kind = p.kind
-- In time order, so only one chunk's indexes are written at a time.
ORDER BY p.played_at;

-- The same listen reported again 3 to 9 s later.
INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT s.user_id, s.track_id, s.artist_id,
       s.played_at + make_interval(secs => 3 + floor(random() * 7)),
       s.source, s.duration_ms, s.listened_ms, s.client_id
FROM scrobbles s
WHERE random() < 0.04
ORDER BY s.played_at;

UPDATE users u SET scrobble_count = c.n, last_seen_at = c.last
FROM (SELECT user_id, count(*) AS n, max(played_at) AS last FROM scrobbles GROUP BY user_id) c
WHERE c.user_id = u.id;
UPDATE tracks t SET scrobble_count = c.n
FROM (SELECT track_id, count(*) AS n FROM scrobbles GROUP BY track_id) c
WHERE c.track_id = t.id;
UPDATE artists a SET scrobble_count = c.n
FROM (SELECT artist_id, count(*) AS n FROM scrobbles GROUP BY artist_id) c
WHERE c.artist_id = a.id;

COMMIT;

CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', NULL, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  NULL, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('user_activity_daily',       NULL, time_bucket('1 day', NOW()));

SELECT count(*) AS scrobbles, count(DISTINCT user_id) AS users,
       count(DISTINCT (user_id, (played_at AT TIME ZONE 'UTC')::date)) AS user_days
FROM scrobbles;
