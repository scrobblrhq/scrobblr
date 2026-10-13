-- A follow graph for the synthetic population (population.sql), and albums
-- for its catalog, for measuring the social routes (docs/social.md):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/follows.sql
--
-- Deterministic. Each artist's 8 tracks go on two albums of 4 (the
-- catalog's album of a track; scrobbles keep naming none). Every account
-- follows a number of others drawn from a heavy-tailed distribution
-- (median 35, p90 121, p99 1,007, capped at the server's 2,000: 314,903
-- follows over 4,000 accounts), picked with
-- probability falling with the followee's popularity rank (Zipf: the most
-- followed account has 3,721 followers), at random times over 300 days.
-- 5 % of accounts are private and 0.5 % waiting to be erased. Accounts
-- probe10 ... probe2000 follow exactly that many active, public accounts:
-- the measurements' viewers.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE username = 'synth1') THEN
        RAISE EXCEPTION 'run population.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM user_follows) THEN
        RAISE EXCEPTION 'this database has follows already';
    END IF;
END $$;

SELECT setseed(0.17);

BEGIN;

INSERT INTO albums (artist_id, title, title_normalized, image_url)
SELECT a.id, 'Album ' || a.id || side, 'album ' || a.id || side,
       'albums/ab/' || lpad(a.id::text || side, 32, '0') || '.jpg'
FROM artists a CROSS JOIN (VALUES ('a'), ('b')) s(side);

UPDATE tracks t SET album_id = al.id
FROM (SELECT id, artist_id, row_number() OVER (PARTITION BY artist_id ORDER BY id) AS n
      FROM tracks) r
JOIN albums al ON al.artist_id = r.artist_id
              AND al.title_normalized = 'album ' || r.artist_id || CASE WHEN r.n <= 4 THEN 'a' ELSE 'b' END
WHERE t.id = r.id;

-- Popularity: a random rank per account.
CREATE TEMP TABLE synth_popularity AS
SELECT id, row_number() OVER (ORDER BY random()) AS rank
FROM users;

CREATE TEMP TABLE synth_degree AS
SELECT id, LEAST(2000, floor(20 / power(draw, 0.8)))::int AS following
FROM (SELECT id, random() AS draw FROM users) d;

-- Weighted sampling without replacement (Efraimidis-Spirakis): the
-- smallest -ln(u) / weight per follower. Drawn in a subquery, so random()
-- runs per pair.
INSERT INTO user_follows (follower_id, followee_id, created_at)
SELECT follower_id, followee_id, NOW() - make_interval(secs => floor(random() * 300 * 86400))
FROM (
    SELECT f.id AS follower_id, p.id AS followee_id, d.following,
           row_number() OVER (PARTITION BY f.id
                              ORDER BY -ln(1 - random()) * power(p.rank, 0.9)) AS pick
    FROM users f
    JOIN synth_degree d ON d.id = f.id
    CROSS JOIN synth_popularity p
    WHERE p.id <> f.id
) ranked
WHERE pick <= following;

UPDATE users SET is_private = TRUE
WHERE id IN (SELECT id FROM users ORDER BY random() LIMIT (SELECT count(*) / 20 FROM users));
UPDATE users SET delete_after = NOW() + INTERVAL '5 days'
WHERE id IN (SELECT id FROM users WHERE NOT is_private ORDER BY random()
             LIMIT (SELECT count(*) / 200 FROM users));

-- Probes: established accounts with no history of their own, following
-- active public accounts.
INSERT INTO users (username, email, password_hash, created_at)
SELECT 'probe' || n, 'probe' || n || '@synthetic.invalid', 'x', NOW() - INTERVAL '400 days'
FROM unnest(ARRAY[10, 100, 500, 1000, 2000]) n;

INSERT INTO user_follows (follower_id, followee_id, created_at)
SELECT p.id, f.id, NOW() - make_interval(secs => floor(random() * 300 * 86400))
FROM users p
CROSS JOIN LATERAL (
    SELECT u.id FROM users u
    WHERE u.username LIKE 'synth%' AND NOT u.is_private AND u.delete_after IS NULL
    ORDER BY random() + p.id * 0
    LIMIT substr(p.username, 6)::int
) f
WHERE p.username LIKE 'probe%';

COMMIT;

ANALYZE user_follows;
ANALYZE users;
ANALYZE tracks;

SELECT sum(following) AS follows,
       percentile_disc(ARRAY[0.5, 0.9, 0.99]) WITHIN GROUP (ORDER BY following) AS following_p50_p90_p99,
       max(following) AS following_max,
       (SELECT max(n) FROM (SELECT count(*) AS n FROM user_follows GROUP BY followee_id) x)
           AS followers_max
FROM (SELECT follower_id, count(*) AS following FROM user_follows GROUP BY follower_id) f;
