-- Attacks on album attribution (docs/social.md, Albums), on the synthetic
-- population with albums (albums.sql), before `worker albums backfill`:
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/album-attack.sql
--   DATABASE_URL="$SCRATCH_URL" worker albums backfill
--   psql "$SCRATCH_URL" -f scripts/synthetic/album-attack-results.sql
--
-- Every farm account is established (registered 60 days ago) and is
-- assumed to have escaped the coordination detection: the worst case.
-- Deterministic.
--
--   wide    2,000 accounts play the track most accounts file under its
--           album once each, from a fake album;
--   loop    one account plays it 1,000 times in a day from another;
--   short   a track about 100 accounts file under its album gets a farm
--           one account short of half again their weight;
--   enough  another gets half again their weight.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM scrobbles WHERE album_id IS NOT NULL LIMIT 1) THEN
        RAISE EXCEPTION 'run albums.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM users WHERE username LIKE 'albumfarm%') THEN
        RAISE EXCEPTION 'the attacks are there already';
    END IF;
END $$;

BEGIN;

-- What each album of a track weighs, as the worker weighs it: each
-- established account's say (its latest play naming an album), at its
-- lowest coordination factor of the last 28 days, in thousandths.
CREATE TEMP TABLE synth_weights AS
SELECT s.track_id, s.album_id, count(*) AS accounts,
       sum(COALESCE((SELECT min(d.factor) FROM coordination_days d
                     WHERE d.user_id = s.user_id AND d.day >= CURRENT_DATE - 28), 1000)) AS weight
FROM (
    SELECT DISTINCT ON (s.track_id, s.user_id) s.track_id, s.user_id, s.album_id
    FROM scrobbles s
    JOIN users u ON u.id = s.user_id
    WHERE s.album_id IS NOT NULL AND u.created_at <= NOW() - INTERVAL '7 days'
      AND u.delete_after IS NULL
    ORDER BY s.track_id, s.user_id, s.played_at DESC
) s
GROUP BY 1, 2;

-- Tracks whose album leads already, so that only an attack can move them.
CREATE TEMP TABLE synth_leading AS
SELECT w.track_id, w.album_id, w.accounts, w.weight
FROM (SELECT *, max(weight) OVER (PARTITION BY track_id) AS best FROM synth_weights) w
JOIN tracks t ON t.id = w.track_id AND t.album_id = w.album_id
WHERE w.weight = w.best;

CREATE TEMP TABLE synth_targets (attack TEXT, track_id BIGINT, album_id BIGINT, accounts INT, weight BIGINT, farm INT);

INSERT INTO synth_targets
SELECT 'wide', track_id, album_id, accounts, weight, 2000
FROM synth_leading ORDER BY weight DESC, track_id LIMIT 1;
INSERT INTO synth_targets
SELECT 'loop', track_id, album_id, accounts, weight, 1 FROM synth_targets WHERE attack = 'wide';

-- An album needs 1.5 times the current one's weight (and one account more).
WITH mid AS (
    SELECT * FROM synth_leading WHERE accounts BETWEEN 95 AND 105 ORDER BY track_id LIMIT 2
)
INSERT INTO synth_targets
SELECT CASE WHEN row_number() OVER (ORDER BY track_id) = 1 THEN 'short' ELSE 'enough' END,
       track_id, album_id, accounts, weight,
       ceil(weight * 1.5 / 1000)::int - CASE WHEN row_number() OVER (ORDER BY track_id) = 1 THEN 1 ELSE 0 END
FROM mid;

INSERT INTO albums (artist_id, title, title_normalized)
SELECT DISTINCT t.artist_id, 'Fake ' || g.attack, 'fake ' || g.attack
FROM synth_targets g JOIN tracks t ON t.id = g.track_id;

INSERT INTO users (username, email, password_hash, created_at)
SELECT 'albumfarm_' || g.attack || n, 'albumfarm_' || g.attack || n || '@synthetic.invalid', 'x',
       NOW() - INTERVAL '60 days'
FROM synth_targets g CROSS JOIN LATERAL generate_series(1, g.farm) n;

INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source, duration_ms, listened_ms)
SELECT u.id, t.id, t.artist_id, f.id,
       date_trunc('day', NOW()) - INTERVAL '1 day' + make_interval(secs => p * 60),
       'extension', 240000, 240000
FROM synth_targets g
JOIN tracks t ON t.id = g.track_id
JOIN albums f ON f.artist_id = t.artist_id AND f.title_normalized = 'fake ' || g.attack
JOIN users u ON u.username LIKE 'albumfarm\_' || g.attack || '%'
CROSS JOIN LATERAL generate_series(1, CASE WHEN g.attack = 'loop' THEN 1000 ELSE 1 END) p;

CREATE TABLE synth_album_attacks AS SELECT * FROM synth_targets;

COMMIT;

SELECT a.attack, a.track_id, t.title, al.title AS album, a.accounts AS its_accounts,
       round(a.weight / 1000.0, 3) AS its_weight, a.farm
FROM synth_album_attacks a JOIN tracks t ON t.id = a.track_id JOIN albums al ON al.id = a.album_id
ORDER BY a.attack;
