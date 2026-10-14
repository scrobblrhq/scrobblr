-- Farms trying to make an artist of theirs "similar" to a popular one, for
-- measuring artist similarity and recommendations (docs/rankings.md,
-- Similarity), on top of scripts/synthetic/population.sql run with
-- -v genres=20 (it refuses a database without that population):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/similarity-attack.sql
--
-- Run it as a superuser (it skips triggers). Every farm account is older
-- than the 7 days new accounts need, claims a client the weights trust,
-- declares listened time at the scrobble point and plays 15 to 40 tracks a
-- day on 92 % of the last 28 days: 60 % its target and the popular artist
-- (half each), the rest filler.
--
-- - a, aged farm (:aged accounts) registered within 2 days 40 days ago,
--   before registration origins were recorded: "Similarity Target A" with
--   the most listened-to artist; filler from the 50 most listened to.
-- - b, aged farm (:aged_mid) registered within 2 days 45 days ago:
--   "Similarity Target B" with the head of a smaller genre; filler from
--   that genre's 30 most listened to, as its fans would play.
-- - c, established farm (:established) registered over 840 days like the
--   population, origins long expired: "Similarity Target C" with the most
--   listened-to artist. Nothing about the accounts stands out.
-- - d, established farm (:established_mid): "Similarity Target D" with a
--   genre's 10th artist (about 250 listeners); genre filler.
-- - e, established farm (:established_small): "Similarity Target E" with
--   another genre's 10th artist; genre filler.
-- - f, varied farm (:varied_mid) established like c, each account with 20
--   favourites of its own drawn from the genre like an honest fan's:
--   "Similarity Target F" with the same artist as d. Its accounts don't
--   copy each other.
-- - g, varied farm (:varied) the same with the most listened-to artist:
--   "Similarity Target G".
--
-- 30 % of the population lists itself as taste neighbours (every third
-- account). synthetic_scenarios records who is in which farm,
-- synthetic_targets each farm's target and popular artist. Afterwards
-- refresh the aggregates (end of this file), classify, weigh, detect and
-- recompute (docs/rankings.md), then
-- scripts/synthetic/similarity-attack-results.sql.

\set ON_ERROR_STOP on
\if :{?aged}
\else
  \set aged 400
\endif
\if :{?aged_mid}
\else
  \set aged_mid 300
\endif
\if :{?established}
\else
  \set established 300
\endif
\if :{?established_mid}
\else
  \set established_mid 100
\endif
\if :{?established_small}
\else
  \set established_small 15
\endif
\if :{?varied_mid}
\else
  \set varied_mid 100
\endif
\if :{?varied}
\else
  \set varied 300
\endif

DO $$
BEGIN
    IF to_regclass('synthetic_genres') IS NULL THEN
        RAISE EXCEPTION 'run scripts/synthetic/population.sql -v genres=20 on this database first';
    END IF;
    IF EXISTS (SELECT 1 FROM artists WHERE name = 'Similarity Target A') THEN
        RAISE EXCEPTION 'the similarity attacks are already in this database';
    END IF;
END $$;

SELECT setseed(0.31);

BEGIN;

SET LOCAL session_replication_role = replica;

CREATE TABLE IF NOT EXISTS synthetic_scenarios (
    user_id   BIGINT  NOT NULL,
    scenario  TEXT    NOT NULL,
    PRIMARY KEY (user_id, scenario)
);
CREATE TABLE synthetic_similarity_targets (
    scenario    TEXT    PRIMARY KEY,
    target_id   BIGINT  NOT NULL,
    popular_id  BIGINT  NOT NULL
);

UPDATE users SET similarity = 'listed', similarity_changed_at = NOW()
WHERE username ~ '^synth[0-9]+$' AND substring(username FROM 6)::int % 3 = 0;

INSERT INTO scrobble_clients (protocol, name, verified)
VALUES ('scrobblr', 'ytmusic', FALSE)
ON CONFLICT DO NOTHING;

-- Who listened to what over the last 28 days, before the farms.
CREATE TEMP TABLE audience AS
SELECT d.artist_id, count(DISTINCT d.user_id) AS listeners
FROM scrobbles_daily_by_artist d
WHERE d.day >= CURRENT_DATE - 28
GROUP BY 1;
CREATE TEMP TABLE genre_order AS
SELECT g.genre, a.artist_id, a.listeners,
       row_number() OVER (PARTITION BY g.genre ORDER BY a.listeners DESC, a.artist_id) AS r
FROM audience a JOIN synthetic_artist_genres g ON g.artist_id = a.artist_id;

-- The most listened-to artist; the head of the genre whose head has the
-- fewest listeners; the 10th artists of genres 4 and 0.
CREATE TEMP TABLE popular AS
SELECT 'a: aged farm, top' AS scenario,
       (SELECT artist_id FROM audience ORDER BY listeners DESC, artist_id LIMIT 1) AS artist_id
UNION ALL
SELECT 'b: aged farm, genre head',
       (SELECT artist_id FROM genre_order WHERE r = 1 ORDER BY listeners, artist_id LIMIT 1)
UNION ALL
SELECT 'c: established farm, top',
       (SELECT artist_id FROM audience ORDER BY listeners DESC, artist_id LIMIT 1)
UNION ALL
SELECT 'd: established farm, 10th of a genre',
       (SELECT artist_id FROM genre_order WHERE genre = 4 AND r = 10)
UNION ALL
SELECT 'e: established farm, small',
       (SELECT artist_id FROM genre_order WHERE genre = 0 AND r = 10)
UNION ALL
SELECT 'f: varied farm, 10th of a genre',
       (SELECT artist_id FROM genre_order WHERE genre = 4 AND r = 10)
UNION ALL
SELECT 'g: varied farm, top',
       (SELECT artist_id FROM audience ORDER BY listeners DESC, artist_id LIMIT 1);

INSERT INTO artists (name, name_normalized, enriched_at)
SELECT 'Similarity Target ' || t, 'similarity target ' || lower(t), NOW()
FROM unnest(ARRAY['A', 'B', 'C', 'D', 'E', 'F', 'G']) t;

INSERT INTO synthetic_similarity_targets (scenario, target_id, popular_id)
SELECT p.scenario, a.id, p.artist_id
FROM popular p
JOIN artists a ON a.name = 'Similarity Target ' || upper(left(p.scenario, 1));

INSERT INTO tracks (artist_id, title, title_normalized, duration_ms, mb_duration_ms,
                    enriched_at, lastfm_checked_at)
SELECT a.id, a.name || ' ' || n, lower(a.name) || ' ' || n,
       170000 + (n * 7919 + a.id * 104729) % 70000, 170000 + (n * 7919 + a.id * 104729) % 70000,
       NOW(), NOW()
FROM artists a
JOIN synthetic_similarity_targets st ON st.target_id = a.id
CROSS JOIN generate_series(1, 8) n;

INSERT INTO track_artists (track_id, artist_id, role, position)
SELECT t.id, t.artist_id, 'primary', 0
FROM tracks t JOIN synthetic_similarity_targets st ON st.target_id = t.artist_id;

CREATE TEMP TABLE farm_tracks AS
SELECT t.artist_id, (row_number() OVER (PARTITION BY t.artist_id ORDER BY t.id) - 1)::int AS n,
       t.id, COALESCE(t.duration_ms, (120000 + (t.id * 7919) % 240000)::int) AS length
FROM tracks t
WHERE t.artist_id IN (SELECT target_id FROM synthetic_similarity_targets
                      UNION SELECT popular_id FROM synthetic_similarity_targets
                      UNION SELECT artist_id FROM (SELECT artist_id FROM audience
                                                   ORDER BY listeners DESC, artist_id LIMIT 50) top
                      UNION SELECT artist_id FROM genre_order WHERE r <= 30
                      UNION SELECT g.artist_id FROM synthetic_artist_genres g
                            JOIN synthetic_artist_genres p ON p.genre = g.genre
                            JOIN synthetic_similarity_targets st ON st.popular_id = p.artist_id
                            WHERE left(st.scenario, 1) IN ('f', 'g'));
CREATE INDEX ON farm_tracks (artist_id, n);

-- Each farm's filler: the 50 most listened-to artists, or the 30 of the
-- popular artist's genre.
CREATE TEMP TABLE filler AS
SELECT st.scenario, (row_number() OVER (PARTITION BY st.scenario ORDER BY f.artist_id) - 1)::int AS k,
       f.artist_id
FROM synthetic_similarity_targets st
CROSS JOIN LATERAL (
    SELECT artist_id FROM audience
    WHERE left(st.scenario, 1) IN ('a', 'c')
    ORDER BY listeners DESC, artist_id LIMIT 50
) f
UNION ALL
SELECT st.scenario, (row_number() OVER (PARTITION BY st.scenario ORDER BY o.artist_id) - 1)::int,
       o.artist_id
FROM synthetic_similarity_targets st
JOIN synthetic_artist_genres g ON g.artist_id = st.popular_id
JOIN genre_order o ON o.genre = g.genre AND o.r <= 30
WHERE left(st.scenario, 1) IN ('b', 'd', 'e');
CREATE INDEX ON filler (scenario, k);
CREATE TEMP TABLE filler_size AS SELECT scenario, count(*) AS n FROM filler GROUP BY 1;

INSERT INTO users (username, email, password_hash, created_at)
SELECT 'simaged' || n, 'simaged' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 40 + n % 2, secs => (n * 104729) % 86400)
FROM generate_series(1, :aged) n
UNION ALL
SELECT 'simagedmid' || n, 'simagedmid' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 45 + n % 2, secs => (n * 104729) % 86400)
FROM generate_series(1, :aged_mid) n
UNION ALL
SELECT 'simest' || n, 'simest' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 7919) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :established) n
UNION ALL
SELECT 'simestmid' || n, 'simestmid' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 6007) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :established_mid) n
UNION ALL
SELECT 'simestsmall' || n, 'simestsmall' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 4111) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :established_small) n
UNION ALL
SELECT 'simvariedmid' || n, 'simvariedmid' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 3089) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :varied_mid) n
UNION ALL
SELECT 'simvaried' || n, 'simvaried' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 2017) % 841, secs => (n * 104729) % 86400)
FROM generate_series(1, :varied) n;

CREATE TEMP TABLE farm_accounts AS
SELECT u.id AS user_id, st.scenario, st.target_id, st.popular_id
FROM users u
JOIN synthetic_similarity_targets st ON left(st.scenario, 1) = CASE
    WHEN u.username ~ '^simaged[0-9]+$' THEN 'a'
    WHEN u.username ~ '^simagedmid[0-9]+$' THEN 'b'
    WHEN u.username ~ '^simest[0-9]+$' THEN 'c'
    WHEN u.username ~ '^simestmid[0-9]+$' THEN 'd'
    WHEN u.username ~ '^simestsmall[0-9]+$' THEN 'e'
    WHEN u.username ~ '^simvariedmid[0-9]+$' THEN 'f'
    WHEN u.username ~ '^simvaried[0-9]+$' THEN 'g' END;

-- The varied farms' favourites: 20 each from the popular artist's genre,
-- Zipf-like by rank as the population draws them.
CREATE TEMP TABLE varied_favourites AS
SELECT d.user_id, d.slot, ga.artist_id
FROM (
    SELECT fa.user_id, g.genre, s.slot, random() + s.slot * 0 AS u
    FROM farm_accounts fa
    JOIN synthetic_artist_genres g ON g.artist_id = fa.popular_id
    CROSS JOIN generate_series(0, 19) s(slot)
    WHERE left(fa.scenario, 1) IN ('f', 'g')
) d
JOIN (SELECT genre, count(*) AS size FROM synthetic_artist_genres GROUP BY 1) z ON z.genre = d.genre
JOIN (SELECT genre, artist_id, row_number() OVER (PARTITION BY genre ORDER BY rank) AS genre_rank
      FROM synthetic_artist_genres) ga
  ON ga.genre = d.genre AND ga.genre_rank = LEAST(z.size, floor(power(z.size, d.u)))::int;
CREATE INDEX ON varied_favourites (user_id, slot);

INSERT INTO synthetic_scenarios (user_id, scenario)
SELECT user_id, 'similarity ' || scenario FROM farm_accounts;

CREATE TEMP TABLE farm_plays AS
WITH farm_days AS (
    SELECT * FROM (
        SELECT fa.*, d::date AS day, random() AS draw, 15 + floor(random() * 26)::int AS plays,
               (7 + floor(random() * 15)) * 3600 + floor(random() * 3600) AS start_secs
        FROM farm_accounts fa
        CROSS JOIN generate_series(CURRENT_DATE - 28, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE draw < 0.92
), slots AS (
    SELECT fd.user_id, fd.scenario, fd.target_id, fd.popular_id, fd.day, fd.start_secs, seq,
           random() AS pick, floor(random() * 1000000)::int AS r,
           1 + LEAST(7, floor(power(8, random()))::int - 1) AS track_rank
    FROM farm_days fd
    CROSS JOIN LATERAL generate_series(0, fd.plays - 1) seq
), picked AS (
    SELECT s.user_id, s.day, s.start_secs, s.seq,
           CASE WHEN s.pick < 0.3 THEN s.target_id
                WHEN s.pick < 0.6 THEN s.popular_id
                ELSE COALESCE(v.artist_id, f.artist_id) END AS artist_id,
           s.r % 8 AS n
    FROM slots s
    LEFT JOIN filler_size z ON z.scenario = s.scenario
    LEFT JOIN filler f ON f.scenario = s.scenario AND f.k = s.r % z.n
    LEFT JOIN varied_favourites v ON v.user_id = s.user_id AND v.slot = s.r % 20
)
SELECT p.user_id, p.day, p.seq, t.id AS track_id, t.artist_id, t.length,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(t.length + 4000) OVER w - t.length - 4000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
JOIN farm_tracks t ON t.artist_id = p.artist_id AND t.n = p.n
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT p.user_id, p.track_id, p.artist_id, p.played_at, 'ytmusic', p.length,
       LEAST(p.length / 2, 240000) + 500, c.id
FROM farm_plays p
JOIN scrobble_clients c ON c.protocol = 'scrobblr' AND c.name = 'ytmusic' AND NOT c.verified
ORDER BY p.played_at;

-- Registration origins as the API records them: the aged farms registered
-- before the key was set, the established accounts long before the 90
-- days origins are kept; the population as coordination.sql records it.
INSERT INTO registration_origins (user_id, key_id, address, network, created_at)
SELECT u.id, 1, hashtextextended('address:' || u.id, 0),
       hashtextextended('net:' || (u.id * 7919) % 2000, 0), u.created_at
FROM users u
WHERE u.created_at >= NOW() - INTERVAL '90 days'
  AND u.username ~ '^synth[0-9]+$'
ON CONFLICT (user_id) DO NOTHING;

UPDATE users u SET scrobble_count = u.scrobble_count + c.n, last_seen_at = GREATEST(u.last_seen_at, c.last)
FROM (SELECT user_id, count(*) AS n, max(played_at) AS last FROM scrobbles
      WHERE user_id IN (SELECT user_id FROM farm_accounts) GROUP BY user_id) c
WHERE c.user_id = u.id;

COMMIT;

CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', CURRENT_DATE - 29, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  CURRENT_DATE - 29, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('user_activity_daily',       CURRENT_DATE - 29, time_bucket('1 day', NOW()));

SELECT st.scenario, a.name AS popular, au.listeners AS popular_listeners_28d,
       (SELECT count(*) FROM farm_accounts f WHERE f.scenario = st.scenario) AS accounts
FROM synthetic_similarity_targets st
JOIN artists a ON a.id = st.popular_id
JOIN audience au ON au.artist_id = st.popular_id
ORDER BY 1;
SELECT count(*) AS scrobbles, count(DISTINCT user_id) AS users FROM scrobbles;
