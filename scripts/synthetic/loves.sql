-- Loved tracks for the synthetic population (population.sql), for
-- measuring the loved-track routes (docs/social.md, Loved tracks):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/loves.sql
--
-- Deterministic. Every synthetic account loves some of the tracks it
-- played, a number drawn log-normally (median 40, a few thousand at most),
-- loved at a random time after it first played them; 30 % arrive as
-- imported. One more account, `lover`, loves 49,999 tracks over three
-- years, one short of the cap: the worst case of a list and of the cap's
-- count. Prints the numbers and the table's size.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE username = 'synth1') THEN
        RAISE EXCEPTION 'run population.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM loved_tracks) THEN
        RAISE EXCEPTION 'this database has loves already';
    END IF;
END $$;

SELECT setseed(0.34);

CREATE TEMP TABLE want AS
SELECT id AS user_id,
       LEAST(5000, GREATEST(1, floor(exp(ln(40) + 1.2 * random_normal()))))::int AS n
FROM users
WHERE username LIKE 'synth%';

INSERT INTO loved_tracks (user_id, track_id, loved_at, source)
SELECT p.user_id, p.track_id,
       p.first_played + p.r * (NOW() - p.first_played),
       CASE WHEN p.s < 0.3 THEN 'lastfm_import' ELSE 'scrobblr' END
FROM (
    SELECT d.user_id, d.track_id, d.first_played, random() AS r, random() AS s,
           row_number() OVER (PARTITION BY d.user_id ORDER BY random()) AS rn
    FROM (
        SELECT user_id, track_id, min(played_at) AS first_played
        FROM scrobbles
        GROUP BY 1, 2
    ) d
) p
JOIN want w ON w.user_id = p.user_id
WHERE p.rn <= w.n;

INSERT INTO users (username, email, password_hash, created_at)
VALUES ('lover', 'lover@synthetic.test', 'x', NOW() - INTERVAL '1200 days');

INSERT INTO loved_tracks (user_id, track_id, loved_at, source)
SELECT u.id, t.id, NOW() - t.r * INTERVAL '1100 days', 'lastfm_import'
FROM users u,
     (SELECT id, random() AS r FROM tracks ORDER BY random() LIMIT 49999) t
WHERE u.username = 'lover';

VACUUM ANALYZE loved_tracks;

SELECT sum(n) AS loves,
       count(*) AS accounts,
       percentile_disc(0.5) WITHIN GROUP (ORDER BY n) AS median_per_account,
       percentile_disc(0.9) WITHIN GROUP (ORDER BY n) AS p90,
       max(n) AS most
FROM (SELECT user_id, count(*) AS n FROM loved_tracks GROUP BY 1) per_account;

SELECT pg_size_pretty(pg_total_relation_size('loved_tracks')) AS total,
       pg_size_pretty(pg_relation_size('loved_tracks')) AS heap,
       pg_size_pretty(pg_indexes_size('loved_tracks')) AS indexes;
