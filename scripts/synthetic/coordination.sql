-- Coordinated farms and honest crowds, for measuring the coordination
-- detection (docs/rankings.md), on top of scripts/synthetic/population.sql
-- (it refuses a database without that population):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/coordination.sql
--
-- Run it as a superuser (it skips triggers). Every farm account is older
-- than the 7 days new accounts need, claims a client the weights trust,
-- declares listened time at the scrobble point, and plays 15 to 40 tracks
-- a day over the last 28 days: three target artists, and filler.
--
-- - a, aged farm (:aged accounts): registered within 2 days, 40 days ago,
--   before registration origins were recorded; each account at its own
--   hours; 70 % targets, filler from the 50 most popular artists.
-- - b, decoy farm (:decoy): registered within 2 days, 50 days ago, no
--   origins; 25 % targets, the rest from 25 favourites of its own drawn
--   like the population's, so it looks as diverse as anyone.
-- - c, proxied farm (:proxied): registered within 3 days, 45 days ago,
--   from 3 addresses in one /24; one box runs every account on one
--   schedule (20:00, same tracks in the same order) through its own
--   Last.fm API key, as a pylast script would.
-- - d, spread farm (:spread): registered over 4 months (60 to 180 days
--   ago, so most before origins are kept), each from its own residential
--   address, at its own hours; 50 % targets, favourites as filler.
-- - e, release: :release honest users play a new album the night it comes
--   out (40 % in its first 10 minutes), then some replay it; :hype new
--   accounts sign up the week before for it.
-- - f, friends and shared networks: 40 groups of 4 friends registered the
--   same days, sharing a playlist and a Saturday listening party; 30
--   households of 3 behind one address, listening together most evenings;
--   150 students behind one campus address, registered within 2 weeks.
-- - g, fandom: :fans honest users of a small artist, 6 to 12 plays most
--   nights for 60 days, and one listening party.
--
-- synthetic_scenarios records who is in which, synthetic_targets the
-- artists each scenario is about. Afterwards refresh the aggregates (end of
-- this file), classify, weigh, detect and report (docs/rankings.md), then
-- scripts/synthetic/coordination-results.sql.

\set ON_ERROR_STOP on
\if :{?aged}
\else
  \set aged 400
\endif
\if :{?decoy}
\else
  \set decoy 400
\endif
\if :{?proxied}
\else
  \set proxied 300
\endif
\if :{?spread}
\else
  \set spread 300
\endif
\if :{?release}
\else
  \set release 2500
\endif
\if :{?hype}
\else
  \set hype 150
\endif
\if :{?fans}
\else
  \set fans 60
\endif

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE email LIKE 'synth%@synthetic.invalid') THEN
        RAISE EXCEPTION 'run scripts/synthetic/population.sql on this database first';
    END IF;
    IF EXISTS (SELECT 1 FROM artists WHERE name = 'Aged Target 1') THEN
        RAISE EXCEPTION 'the coordination scenarios are already in this database';
    END IF;
END $$;

SELECT setseed(0.23);

BEGIN;

-- Counters and aggregates are refreshed in bulk; skipping triggers needs a
-- superuser.
SET LOCAL session_replication_role = replica;

CREATE TABLE synthetic_scenarios (
    user_id   BIGINT  NOT NULL,
    scenario  TEXT    NOT NULL,
    PRIMARY KEY (user_id, scenario)
);
CREATE TABLE synthetic_targets (
    artist_id  BIGINT  PRIMARY KEY,
    scenario   TEXT    NOT NULL
);

INSERT INTO scrobble_clients (protocol, name, verified)
VALUES ('scrobblr', 'ytmusic', FALSE),
       ('lastfm', 'f4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c9', FALSE)
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE synth_artists AS
SELECT substring(name FROM 14)::int AS rank, id FROM artists WHERE name ~ '^Synth Artist [0-9]+$';
CREATE INDEX ON synth_artists (rank);
SELECT max(rank) AS artists FROM synth_artists \gset
CREATE TEMP TABLE synth_tracks AS
SELECT t.artist_id, row_number() OVER (PARTITION BY t.artist_id ORDER BY t.id) AS rank, t.id,
       COALESCE(t.duration_ms, (120000 + (t.id * 7919) % 240000)::int) AS length,
       t.duration_ms IS NOT NULL AS known
FROM tracks t JOIN synth_artists a ON a.id = t.artist_id;
CREATE INDEX ON synth_tracks (artist_id, rank);

-- The population's users, by number, with the client each listens through.
CREATE TEMP TABLE honest AS
SELECT u.id AS user_id, substring(u.username FROM 6)::int AS n, c.client_id, c.source
FROM users u
CROSS JOIN LATERAL (
    SELECT s.client_id, s.source FROM scrobbles s
    WHERE s.user_id = u.id ORDER BY s.played_at DESC LIMIT 1
) c
WHERE u.username ~ '^synth[0-9]+$';
CREATE INDEX ON honest (n);

INSERT INTO artists (name, name_normalized, enriched_at)
SELECT name, lower(name), NOW()
FROM unnest(ARRAY[
    'Aged Target 1', 'Aged Target 2', 'Aged Target 3',
    'Decoy Target 1', 'Decoy Target 2', 'Decoy Target 3',
    'Proxied Target 1', 'Proxied Target 2', 'Proxied Target 3',
    'Spread Target 1', 'Spread Target 2', 'Spread Target 3',
    'Release Star', 'Indie Darling']) name;

INSERT INTO synthetic_targets (artist_id, scenario)
SELECT id, CASE
    WHEN name LIKE 'Aged %' THEN 'a: aged farm'
    WHEN name LIKE 'Decoy %' THEN 'b: decoy farm'
    WHEN name LIKE 'Proxied %' THEN 'c: proxied farm'
    WHEN name LIKE 'Spread %' THEN 'd: spread farm'
    WHEN name = 'Release Star' THEN 'e: release'
    ELSE 'g: fandom' END
FROM artists
WHERE name ~ '^(Aged|Decoy|Proxied|Spread) Target [123]$' OR name IN ('Release Star', 'Indie Darling');

INSERT INTO tracks (artist_id, title, title_normalized, duration_ms, mb_duration_ms,
                    enriched_at, lastfm_checked_at)
SELECT a.id, a.name || ' ' || n, lower(a.name) || ' ' || n,
       170000 + (n * 7919 + a.id * 104729) % 70000, 170000 + (n * 7919 + a.id * 104729) % 70000,
       NOW(), NOW()
FROM artists a
JOIN synthetic_targets st ON st.artist_id = a.id
CROSS JOIN generate_series(1, 10) n
WHERE n <= 8 OR a.name = 'Release Star';

INSERT INTO track_artists (track_id, artist_id, role, position)
SELECT t.id, t.artist_id, 'primary', 0
FROM tracks t JOIN synthetic_targets st ON st.artist_id = t.artist_id;

CREATE TEMP TABLE scenario_tracks AS
SELECT a.name AS artist, t.id, t.artist_id, t.duration_ms AS length,
       (row_number() OVER (PARTITION BY a.id ORDER BY t.id) - 1)::int AS n
FROM tracks t
JOIN artists a ON a.id = t.artist_id
JOIN synthetic_targets st ON st.artist_id = a.id;
CREATE INDEX ON scenario_tracks (artist, n);

-- The 50 most popular artists' tracks, the aged and proxied farms' filler.
CREATE TEMP TABLE filler AS
SELECT (row_number() OVER (ORDER BY t.artist_id, t.rank) - 1)::int AS n, t.id, t.artist_id, t.length
FROM synth_tracks t JOIN synth_artists a ON a.id = t.artist_id
WHERE a.rank <= 50;
CREATE INDEX ON filler (n);

-- ---------------------------------------------------------------------------
--  Farms (a to d)
-- ---------------------------------------------------------------------------

CREATE TEMP TABLE farm_config (
    prefix     TEXT,
    scenario   TEXT,
    targets    TEXT,
    p_target   FLOAT8,
    calqued    BOOLEAN,
    zipf       BOOLEAN,
    lastfm     BOOLEAN
);
INSERT INTO farm_config VALUES
    ('aged',    'a: aged farm',    'Aged Target',    0.70, FALSE, FALSE, FALSE),
    ('decoy',   'b: decoy farm',   'Decoy Target',   0.25, FALSE, TRUE,  FALSE),
    ('proxied', 'c: proxied farm', 'Proxied Target', 0.70, TRUE,  FALSE, TRUE),
    ('spread',  'd: spread farm',  'Spread Target',  0.50, FALSE, TRUE,  FALSE);

INSERT INTO users (username, email, password_hash, created_at)
SELECT 'aged' || n, 'aged' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 40 + n % 2, secs => (n * 104729) % 86400)
FROM generate_series(1, :aged) n
UNION ALL
SELECT 'decoy' || n, 'decoy' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 50 + n % 2, secs => (n * 104729) % 86400)
FROM generate_series(1, :decoy) n
UNION ALL
SELECT 'proxied' || n, 'proxied' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 45 + n % 3, secs => (n * 104729) % 86400)
FROM generate_series(1, :proxied) n
UNION ALL
SELECT 'spread' || n, 'spread' || n || '@synthetic.invalid', 'x',
       NOW() - make_interval(days => 60 + (n * 7919) % 121, secs => (n * 104729) % 86400)
FROM generate_series(1, :spread) n;

CREATE TEMP TABLE farm_accounts AS
SELECT u.id AS user_id, (row_number() OVER (PARTITION BY f.prefix ORDER BY u.id) - 1)::int AS k, f.*
FROM users u
JOIN farm_config f ON u.username ~ ('^' || f.prefix || '[0-9]+$');

INSERT INTO synthetic_scenarios (user_id, scenario)
SELECT user_id, scenario FROM farm_accounts;

CREATE TEMP TABLE farm_favourites AS
SELECT fa.user_id, s.slot, LEAST(:artists, floor(power(:artists, random()))::int) AS artist_rank
FROM farm_accounts fa
CROSS JOIN generate_series(1, 25) s(slot)
WHERE fa.zipf;
CREATE INDEX ON farm_favourites (user_id, slot);

-- A calqued farm draws per (day, play), the same for every account; the
-- others per (account, day, play).
CREATE TEMP TABLE farm_plays AS
WITH farm_days AS (
    SELECT * FROM (
        SELECT fa.*, d::date AS day, random() AS draw,
               CASE WHEN fa.calqued THEN 30 ELSE 15 + floor(random() * 26)::int END AS plays,
               CASE WHEN fa.calqued THEN 20 * 3600 + floor(random() * 30)
                    ELSE (7 + floor(random() * 15)) * 3600 + floor(random() * 3600) END AS start_secs
        FROM farm_accounts fa
        CROSS JOIN generate_series(CURRENT_DATE - 28, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE draw < 0.92
), slots AS (
    SELECT fd.user_id, fd.scenario, fd.targets, fd.zipf, fd.lastfm, fd.day, fd.start_secs, seq,
           CASE WHEN fd.calqued THEN (hashtext(fd.day::text || ':p:' || seq) & 2147483647) % 1000 / 1000.0
                ELSE random() END < fd.p_target AS target,
           CASE WHEN fd.calqued THEN (hashtext(fd.day::text || ':r:' || seq) & 2147483647)
                ELSE floor(random() * 1000000000)::int END AS r,
           1 + LEAST(7, floor(power(8, random()))::int - 1) AS track_rank
    FROM farm_days fd
    CROSS JOIN LATERAL generate_series(0, fd.plays - 1) seq
), picked AS (
    SELECT s.user_id, s.scenario, s.lastfm, s.day, s.start_secs, s.seq,
           COALESCE(tt.id, ff.id, zt.id) AS track_id,
           COALESCE(tt.artist_id, ff.artist_id, zt.artist_id) AS artist_id,
           COALESCE(tt.length, ff.length, zt.length) AS length
    FROM slots s
    LEFT JOIN scenario_tracks tt
           ON s.target AND tt.artist = s.targets || ' ' || (1 + s.r % 3) AND tt.n = (s.r / 3) % 8
    LEFT JOIN filler ff ON NOT s.target AND NOT s.zipf AND ff.n = s.r % 400
    LEFT JOIN farm_favourites fav ON NOT s.target AND s.zipf AND fav.user_id = s.user_id
                                 AND fav.slot = 1 + s.r % 25
    LEFT JOIN synth_artists za ON za.rank = fav.artist_rank
    LEFT JOIN synth_tracks zt ON zt.artist_id = za.id AND zt.rank = s.track_rank
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 4000) OVER w - p.length - 4000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT p.user_id, p.track_id, p.artist_id, p.played_at,
       CASE WHEN p.lastfm THEN 'lastfm' ELSE 'ytmusic' END,
       p.length,
       CASE WHEN p.lastfm THEN NULL ELSE LEAST(p.length / 2, 240000) + 500 END,
       c.id
FROM farm_plays p
JOIN scrobble_clients c
  ON (p.lastfm AND c.protocol = 'lastfm' AND c.name = 'f4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c9')
  OR (NOT p.lastfm AND c.protocol = 'scrobblr' AND c.name = 'ytmusic' AND NOT c.verified)
ORDER BY p.played_at;

-- ---------------------------------------------------------------------------
--  e: a release
-- ---------------------------------------------------------------------------

-- Hype: new accounts registered in the 6 days before the release, each
-- listening to its own favourites every day since, through the extension
-- or the mobile app.
INSERT INTO users (username, email, password_hash, created_at)
SELECT 'hype' || n, 'hype' || n || '@synthetic.invalid', 'x',
       (CURRENT_DATE - 12 + n % 6)::timestamp AT TIME ZONE 'UTC'
           + make_interval(secs => (n * 104729) % 86400)
FROM generate_series(1, :hype) n;

CREATE TEMP TABLE hype AS
SELECT u.id AS user_id, u.created_at,
       CASE WHEN u.id % 2 = 0 THEN 'ytmusic' ELSE 'android' END AS source
FROM users u WHERE u.username ~ '^hype[0-9]+$';

CREATE TEMP TABLE hype_favourites AS
SELECT h.user_id, s.slot, LEAST(:artists, floor(power(:artists, random()))::int) AS artist_rank
FROM hype h CROSS JOIN generate_series(1, 25) s(slot);
CREATE INDEX ON hype_favourites (user_id, slot);

CREATE TEMP TABLE hype_plays AS
WITH hype_days AS (
    SELECT * FROM (
        SELECT h.user_id, h.source, d::date AS day, random() AS draw,
               8 + floor(random() * 45)::int AS plays,
               (6 + floor(random() * 12)) * 3600 + floor(random() * 3600) AS start_secs
        FROM hype h
        CROSS JOIN generate_series(CURRENT_DATE - 12, CURRENT_DATE - 1, INTERVAL '1 day') d
        WHERE d::date > (h.created_at AT TIME ZONE 'UTC')::date
    ) drawn
    WHERE draw < 0.8
), slots AS (
    SELECT hd.*, seq, 1 + floor(random() * 25)::int AS slot,
           1 + LEAST(7, floor(power(8, random()))::int - 1) AS track_rank
    FROM hype_days hd CROSS JOIN LATERAL generate_series(1, hd.plays) seq
), picked AS (
    SELECT s.user_id, s.source, s.day, s.start_secs, s.seq, t.id AS track_id, t.artist_id, t.length
    FROM slots s
    JOIN hype_favourites f ON f.user_id = s.user_id AND f.slot = s.slot
    JOIN synth_artists a ON a.rank = f.artist_rank
    JOIN synth_tracks t ON t.artist_id = a.id AND t.rank = s.track_rank
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 3000) OVER w - p.length - 3000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT p.user_id, p.track_id, p.artist_id, p.played_at, p.source, p.length,
       CASE p.source WHEN 'ytmusic' THEN LEAST(p.length / 2, 240000) + 700
                     ELSE GREATEST(30000, LEAST(p.length / 2, 240000)) + 300 END,
       c.id
FROM hype_plays p
JOIN scrobble_clients c ON c.protocol = 'scrobblr' AND c.name = p.source AND NOT c.verified
ORDER BY p.played_at;

-- Listeners: population users numbered n % 8 < 5 (2,500 of 4,000), up to
-- :release of them, and the hype accounts; each through its usual client.
CREATE TEMP TABLE release_listeners AS
SELECT h.user_id, h.client_id, h.source FROM honest h WHERE h.n % 8 < 5 ORDER BY h.n LIMIT :release;
INSERT INTO release_listeners
SELECT h.user_id, c.id, h.source
FROM hype h JOIN scrobble_clients c ON c.protocol = 'scrobblr' AND c.name = h.source AND NOT c.verified;

INSERT INTO synthetic_scenarios (user_id, scenario)
SELECT user_id, 'e: release' FROM release_listeners
UNION ALL SELECT user_id, 'e: hype signups' FROM hype;

-- The album at its premiere (CURRENT_DATE - 6, 00:00 UTC): in order, 40 %
-- starting within 10 minutes, the rest within 5 hours; then, on each later
-- night, half replay 3 to 10 of its tracks from 00:30.
CREATE TEMP TABLE release_plays AS
WITH sessions AS (
    SELECT l.user_id, l.client_id, l.source, (CURRENT_DATE - 6)::date AS day, 0 AS first,
           10 AS plays,
           CASE WHEN random() < 0.4 THEN floor(random() * 600)
                ELSE floor(random() * 5 * 3600) END AS start_secs
    FROM release_listeners l
    UNION ALL
    SELECT user_id, client_id, source, day, first, plays, start_secs FROM (
        SELECT l.user_id, l.client_id, l.source, d::date AS day, floor(random() * 10)::int AS first,
               3 + floor(random() * 8)::int AS plays, 1800 + floor(random() * 5400) AS start_secs,
               random() AS draw
        FROM release_listeners l
        CROSS JOIN generate_series(CURRENT_DATE - 5, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) replays
    WHERE draw < 0.5
), picked AS (
    SELECT s.user_id, s.client_id, s.source, s.day, s.start_secs, seq, t.id AS track_id,
           t.artist_id, t.length
    FROM sessions s
    CROSS JOIN LATERAL generate_series(0, s.plays - 1) seq
    JOIN scenario_tracks t ON t.artist = 'Release Star' AND t.n = (s.first + seq) % 10
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 3000) OVER w - p.length - 3000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

-- ---------------------------------------------------------------------------
--  f: friends, households, a campus
-- ---------------------------------------------------------------------------

CREATE TEMP TABLE friends AS
SELECT h.user_id, h.client_id, h.source, ((row_number() OVER (ORDER BY h.n) - 1) / 4)::int AS grp
FROM honest h WHERE h.n % 25 = 7 ORDER BY h.n LIMIT 160;
UPDATE users u SET created_at = NOW() - make_interval(days => 62 + f.grp % 25,
                                                      secs => (u.id * 104729) % 172800)
FROM friends f WHERE u.id = f.user_id;

CREATE TEMP TABLE households AS
SELECT h.user_id, h.client_id, h.source, ((row_number() OVER (ORDER BY h.n) - 1) / 3)::int AS hh
FROM honest h WHERE h.n % 25 = 11 ORDER BY h.n LIMIT 90;
UPDATE users u SET created_at = NOW() - make_interval(days => (61 + (u.id * 7919) % 28)::int)
FROM households h WHERE u.id = h.user_id;

CREATE TEMP TABLE campus AS
SELECT h.user_id FROM honest h WHERE h.n % 25 = 13 ORDER BY h.n LIMIT 150;
UPDATE users u SET created_at = NOW() - make_interval(days => (75 + (u.id * 7919) % 14)::int,
                                                      secs => (u.id * 104729) % 86400)
FROM campus c WHERE u.id = c.user_id;

INSERT INTO synthetic_scenarios (user_id, scenario)
SELECT user_id, 'f: friends' FROM friends
UNION ALL SELECT user_id, 'f: households' FROM households
UNION ALL SELECT user_id, 'f: campus' FROM campus;

-- Each group's playlist: 30 tracks of artists ranked 50 to 500. Members
-- play 8 of it on 60 % of nights from 21:30; on Saturdays all four play
-- the same 10 at 22:00.
CREATE TEMP TABLE friend_plays AS
WITH sessions AS (
    SELECT * FROM (
        SELECT f.user_id, f.client_id, f.source, f.grp, d::date AS day,
               extract(isodow FROM d) = 6 AS party, random() AS draw
        FROM friends f
        CROSS JOIN generate_series(CURRENT_DATE - 60, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE party OR draw < 0.6
), slots AS (
    SELECT s.*, seq,
           CASE WHEN s.party THEN 21 * 3600 + 3600 + (s.user_id % 5)
                ELSE 21 * 3600 + 1800 + floor(random() * 3600) END AS start_secs,
           CASE WHEN s.party THEN (s.day - DATE '2026-01-01' + seq) % 30
                ELSE floor(random() * 30)::int END AS slot
    FROM sessions s
    CROSS JOIN LATERAL generate_series(0, CASE WHEN s.party THEN 9 ELSE 7 END) seq
), picked AS (
    SELECT s.user_id, s.client_id, s.source, s.day, s.seq,
           first_value(s.start_secs) OVER (PARTITION BY s.user_id, s.day ORDER BY s.seq) AS start_secs,
           t.id AS track_id, t.artist_id, t.length
    FROM slots s
    JOIN synth_artists a ON a.rank = 50 + (s.grp * 37 + s.slot * 13) % 450
    JOIN synth_tracks t ON t.artist_id = a.id AND t.rank = 1 + (s.grp + s.slot) % 8
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 2000) OVER w - p.length - 2000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

-- Households listen together on half the evenings: the same 6 tracks of
-- their 5 shared artists from 21:30, within 3 seconds of each other.
CREATE TEMP TABLE household_plays AS
WITH evenings AS (
    SELECT * FROM (
        SELECT hh, d::date AS day, random() AS draw, floor(random() * 1800) AS start_secs
        FROM generate_series(0, 29) hh
        CROSS JOIN generate_series(CURRENT_DATE - 60, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE draw < 0.5
), slots AS (
    SELECT e.*, seq, (hashtext(e.day::text || ':' || e.hh || ':' || seq) & 2147483647) AS r
    FROM evenings e CROSS JOIN generate_series(0, 5) seq
), picked AS (
    SELECT h.user_id, h.client_id, h.source, s.day, s.seq,
           21 * 3600 + 1800 + s.start_secs + h.user_id % 3 AS start_secs,
           t.id AS track_id, t.artist_id, t.length
    FROM slots s
    JOIN households h ON h.hh = s.hh
    JOIN synth_artists a ON a.rank = 20 + (s.hh * 53 + s.r % 5 * 17) % 300
    JOIN synth_tracks t ON t.artist_id = a.id AND t.rank = 1 + s.r / 5 % 8
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 2000) OVER w - p.length - 2000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day ORDER BY p.seq);

-- ---------------------------------------------------------------------------
--  g: a fandom
-- ---------------------------------------------------------------------------

CREATE TEMP TABLE fans AS
SELECT h.user_id, h.client_id, h.source FROM honest h WHERE h.n % 25 = 17 ORDER BY h.n LIMIT :fans;
INSERT INTO synthetic_scenarios (user_id, scenario) SELECT user_id, 'g: fandom' FROM fans;

-- 6 to 12 plays on 75 % of nights from 21:30 to 23:30 over 60 days, and a
-- listening party (CURRENT_DATE - 3, 04:00 UTC) with the album in order.
CREATE TEMP TABLE fan_plays AS
WITH sessions AS (
    SELECT * FROM (
        SELECT f.user_id, f.client_id, f.source, d::date AS day, random() AS draw,
               6 + floor(random() * 7)::int AS plays,
               21 * 3600 + 1800 + floor(random() * 7200) AS start_secs, FALSE AS party
        FROM fans f
        CROSS JOIN generate_series(CURRENT_DATE - 60, CURRENT_DATE - 1, INTERVAL '1 day') d
    ) drawn
    WHERE draw < 0.75
    UNION ALL
    SELECT f.user_id, f.client_id, f.source, CURRENT_DATE - 3, 0, 8, 4 * 3600 + f.user_id % 5, TRUE
    FROM fans f
), picked AS (
    SELECT s.user_id, s.client_id, s.source, s.day, s.start_secs, seq, t.id AS track_id,
           t.artist_id, t.length
    FROM sessions s
    CROSS JOIN LATERAL generate_series(0, s.plays - 1) seq
    CROSS JOIN LATERAL (SELECT CASE WHEN s.party THEN seq ELSE floor(random() * 8)::int END AS n) k
    JOIN scenario_tracks t ON t.artist = 'Indie Darling' AND t.n = k.n
)
SELECT p.*,
       (p.day + make_interval(secs => p.start_secs
                                + (sum(p.length + 2000) OVER w - p.length - 2000) / 1000.0))
           AT TIME ZONE 'UTC' AS played_at
FROM picked p
WINDOW w AS (PARTITION BY p.user_id, p.day, p.start_secs ORDER BY p.seq);

-- Honest plays through each user's client, listened time as that client
-- reports it (as in population.sql).
INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                       listened_ms, client_id)
SELECT p.user_id, p.track_id, p.artist_id, p.played_at, p.source, p.length,
       CASE p.source
           WHEN 'ytmusic' THEN LEAST(p.length / 2, 240000) + floor(random() * 2000)::int
           WHEN 'android' THEN GREATEST(30000, LEAST(p.length / 2, 240000)) + floor(random() * 500)::int
           WHEN 'spotify' THEN p.length
           WHEN 'myscript' THEN p.length
       END,
       p.client_id
FROM (
    SELECT user_id, client_id, source, track_id, artist_id, length, played_at FROM release_plays
    UNION ALL
    SELECT user_id, client_id, source, track_id, artist_id, length, played_at FROM friend_plays
    UNION ALL
    SELECT user_id, client_id, source, track_id, artist_id, length, played_at FROM household_plays
    UNION ALL
    SELECT user_id, client_id, source, track_id, artist_id, length, played_at FROM fan_plays
) p
ORDER BY p.played_at;

-- ---------------------------------------------------------------------------
--  Registration origins, as the API records them (REGISTRATION_HASH_KEY):
--  for accounts registered within the 90 days they are kept, except the
--  aged and decoy farms, registered before the key was set. Values are
--  opaque, so a hash of a label stands in for the keyed hash.
-- ---------------------------------------------------------------------------

INSERT INTO registration_origins (user_id, key_id, address, network, created_at)
SELECT u.id, 1,
       CASE WHEN h.user_id IS NOT NULL THEN hashtextextended('household:' || h.hh, 0)
            WHEN c.user_id IS NOT NULL THEN hashtextextended('campus', 0)
            WHEN fa.prefix = 'proxied' THEN hashtextextended('proxy:' || fa.k % 3, 0)
            ELSE hashtextextended('address:' || u.id, 0) END,
       CASE WHEN h.user_id IS NOT NULL THEN hashtextextended('household-net:' || h.hh, 0)
            WHEN c.user_id IS NOT NULL THEN hashtextextended('campus-net', 0)
            WHEN fa.prefix = 'proxied' THEN hashtextextended('proxy-net', 0)
            WHEN fa.prefix = 'spread' THEN hashtextextended('spread-net:' || u.id, 0)
            ELSE hashtextextended('net:' || (u.id * 7919) % 2000, 0) END,
       u.created_at
FROM users u
LEFT JOIN households h ON h.user_id = u.id
LEFT JOIN campus c ON c.user_id = u.id
LEFT JOIN farm_accounts fa ON fa.user_id = u.id
WHERE u.created_at >= NOW() - INTERVAL '90 days'
  AND u.email LIKE '%@synthetic.invalid'
  AND fa.prefix IS DISTINCT FROM 'aged' AND fa.prefix IS DISTINCT FROM 'decoy';

UPDATE users u SET scrobble_count = c.n, last_seen_at = c.last
FROM (SELECT user_id, count(*) AS n, max(played_at) AS last FROM scrobbles
      WHERE played_at >= CURRENT_DATE - 61 GROUP BY user_id) c
WHERE c.user_id = u.id AND u.scrobble_count < c.n;

COMMIT;

CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', CURRENT_DATE - 61, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  CURRENT_DATE - 61, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('user_activity_daily',       CURRENT_DATE - 61, time_bucket('1 day', NOW()));

SELECT s.scenario, count(DISTINCT s.user_id) AS accounts
FROM synthetic_scenarios s GROUP BY 1 ORDER BY 1;
SELECT count(*) AS scrobbles, count(DISTINCT user_id) AS users FROM scrobbles;
