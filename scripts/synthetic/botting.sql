-- Four ways of botting the charts, over the last 7 complete UTC days, on
-- top of scripts/synthetic/population.sql (it refuses a database without
-- that population):
--
--   psql "$SCRATCH_URL" -v farm=1500 -v bots=30 -f scripts/synthetic/botting.sql
--
-- - Farm: :farm accounts registered 3 days ago push "Farm Target", claiming
--   to be the extension and declaring listened time at the scrobble point,
--   backdating plays to the start of the week (the native API takes plays
--   up to 30 days old).
-- - Looper: one established account loops a 3-minute "Loop Target" track
--   8 hours every night.
-- - Import dump: one account imports 50,000 plays of "Dump Target" dated
--   this week, as from a botted Last.fm account.
-- - Short-listen bot: :bots established accounts with an unknown client
--   scrobble "Bot Target" every 35 s, 12 hours a day, declaring 30 s
--   listens (the classifier counts them: 30 s each is within the budget).
--
-- Afterwards refresh the aggregates (end of this file), then classify and
-- weigh (docs/rankings.md).

\set ON_ERROR_STOP on
\if :{?farm}
\else
  \set farm 1500
\endif
\if :{?bots}
\else
  \set bots 30
\endif

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE email LIKE '%@synthetic.invalid') THEN
        RAISE EXCEPTION 'run scripts/synthetic/population.sql on this database first';
    END IF;
    IF EXISTS (SELECT 1 FROM artists WHERE name = 'Farm Target') THEN
        RAISE EXCEPTION 'the botting scenario is already in this database';
    END IF;
END $$;

SELECT setseed(0.17);

BEGIN;

-- Live plays are inserted marked as import -1, which the per-row counter
-- trigger skips (counters are set below, in bulk), then unmarked.

INSERT INTO scrobble_clients (protocol, name, verified)
VALUES ('scrobblr', 'botclient', FALSE)
ON CONFLICT DO NOTHING;

INSERT INTO artists (name, name_normalized, enriched_at)
SELECT name, lower(name), NOW()
FROM unnest(ARRAY['Farm Target', 'Loop Target', 'Dump Target', 'Bot Target']) name;

INSERT INTO tracks (artist_id, title, title_normalized, duration_ms, mb_duration_ms,
                    enriched_at, lastfm_checked_at)
SELECT a.id, a.name || ' ' || n, lower(a.name) || ' ' || n,
       CASE WHEN a.name = 'Loop Target' THEN 180000 ELSE 200000 END,
       CASE WHEN a.name = 'Loop Target' THEN 180000 ELSE 200000 END,
       NOW(), NOW()
FROM artists a
CROSS JOIN generate_series(1, 8) n
WHERE a.name LIKE '% Target';

INSERT INTO track_artists (track_id, artist_id, role, position)
SELECT t.id, t.artist_id, 'primary', 0
FROM tracks t JOIN artists a ON a.id = t.artist_id
WHERE a.name LIKE '% Target';

CREATE TEMP TABLE target_tracks AS
SELECT a.name AS artist, t.id, t.artist_id, t.duration_ms AS length,
       row_number() OVER (PARTITION BY a.id ORDER BY t.id) - 1 AS n
FROM tracks t JOIN artists a ON a.id = t.artist_id
WHERE a.name LIKE '% Target';

INSERT INTO users (username, email, password_hash, created_at)
SELECT 'farm' || n, 'farm' || n || '@synthetic.invalid', 'x', NOW() - INTERVAL '3 days'
FROM generate_series(1, :farm) n
UNION ALL
SELECT 'looper', 'looper@synthetic.invalid', 'x', NOW() - INTERVAL '120 days'
UNION ALL
SELECT 'dumper', 'dumper@synthetic.invalid', 'x', NOW() - INTERVAL '120 days'
UNION ALL
SELECT 'bot' || n, 'bot' || n || '@synthetic.invalid', 'x', NOW() - INTERVAL '120 days'
FROM generate_series(1, :bots) n;

-- Farm: a 20 to 40 play session a day, all 8 tracks in turn.
INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id, import_id)
SELECT u.id, t.id, t.artist_id,
       (d.day + make_interval(secs => s.start + p.seq * 205)) AT TIME ZONE 'UTC',
       'ytmusic', t.length, t.length / 2 + 500,
       (SELECT id FROM scrobble_clients WHERE protocol = 'scrobblr' AND name = 'ytmusic'), -1
FROM users u
CROSS JOIN generate_series(CURRENT_DATE - 7, CURRENT_DATE - 1, INTERVAL '1 day') d(day)
CROSS JOIN LATERAL (SELECT 20 + floor(random() * 21)::int AS plays,
                           (8 + floor(random() * 12)) * 3600 AS start,
                           u.id AS per_user, d.day AS per_day) s
CROSS JOIN LATERAL generate_series(0, s.plays - 1) p(seq)
JOIN target_tracks t ON t.artist = 'Farm Target' AND t.n = p.seq % 8
WHERE u.username LIKE 'farm%';

-- Looper: 160 plays from midnight, every night.
INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id, import_id)
SELECT u.id, t.id, t.artist_id,
       (d.day + make_interval(secs => p.seq * 180)) AT TIME ZONE 'UTC',
       'android', t.length, t.length,
       (SELECT id FROM scrobble_clients WHERE protocol = 'scrobblr' AND name = 'android'), -1
FROM users u
CROSS JOIN generate_series(CURRENT_DATE - 7, CURRENT_DATE - 1, INTERVAL '1 day') d(day)
CROSS JOIN generate_series(0, 159) p(seq)
JOIN target_tracks t ON t.artist = 'Loop Target' AND t.n = 0
WHERE u.username = 'looper';

-- Import dump: 50,000 plays, one every 12 s, as a finished Last.fm import.
INSERT INTO scrobble_imports (user_id, provider, external_user, verified, status,
                              window_from, window_to, fetched, imported, finished_at)
SELECT id, 'lastfm', 'dumpster', TRUE, 'done',
       (CURRENT_DATE - 7)::timestamp AT TIME ZONE 'UTC', CURRENT_DATE::timestamp AT TIME ZONE 'UTC',
       50000, 50000, NOW()
FROM users WHERE username = 'dumper';

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, import_id)
SELECT u.id, t.id, t.artist_id,
       ((CURRENT_DATE - 7)::timestamp + make_interval(secs => p.seq * 12)) AT TIME ZONE 'UTC',
       'lastfm_import', i.id
FROM users u
JOIN scrobble_imports i ON i.user_id = u.id
CROSS JOIN generate_series(0, 49999) p(seq)
JOIN target_tracks t ON t.artist = 'Dump Target' AND t.n = p.seq % 8
WHERE u.username = 'dumper';

-- Short-listen bots: every 35 s from 08:00 to 20:00.
INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id, import_id)
SELECT u.id, t.id, t.artist_id,
       (d.day + make_interval(secs => 8 * 3600 + p.seq * 35 + floor(random() * 3))) AT TIME ZONE 'UTC',
       'botclient', t.length, 30000,
       (SELECT id FROM scrobble_clients WHERE protocol = 'scrobblr' AND name = 'botclient'), -1
FROM users u
CROSS JOIN generate_series(CURRENT_DATE - 7, CURRENT_DATE - 1, INTERVAL '1 day') d(day)
CROSS JOIN generate_series(0, 12 * 3600 / 35 - 1) p(seq)
JOIN target_tracks t ON t.artist = 'Bot Target' AND t.n = p.seq % 8
WHERE u.username LIKE 'bot%';

UPDATE users u SET scrobble_count = c.n, last_seen_at = c.last
FROM (SELECT user_id, count(*) AS n, max(played_at) AS last FROM scrobbles
      WHERE user_id IN (SELECT id FROM users WHERE username ~ '^(farm|bot)[0-9]+$'
                                               OR username IN ('looper', 'dumper'))
      GROUP BY user_id) c
WHERE c.user_id = u.id;
UPDATE tracks t SET scrobble_count = c.n
FROM (SELECT track_id, count(*) AS n FROM scrobbles
      WHERE track_id IN (SELECT id FROM target_tracks) GROUP BY track_id) c
WHERE c.track_id = t.id;
UPDATE artists a SET scrobble_count = c.n
FROM (SELECT artist_id, count(*) AS n FROM scrobbles
      WHERE artist_id IN (SELECT artist_id FROM target_tracks) GROUP BY artist_id) c
WHERE c.artist_id = a.id;

UPDATE scrobbles SET import_id = NULL
WHERE import_id = -1 AND played_at >= (CURRENT_DATE - 7)::timestamp AT TIME ZONE 'UTC';

COMMIT;

CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', CURRENT_DATE - 8, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  CURRENT_DATE - 8, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('user_activity_daily',       CURRENT_DATE - 8, time_bucket('1 day', NOW()));

SELECT a.name, count(*) AS scrobbles, count(DISTINCT s.user_id) AS users
FROM scrobbles s JOIN artists a ON a.id = s.artist_id
WHERE a.name LIKE '% Target'
GROUP BY a.name ORDER BY a.name;
