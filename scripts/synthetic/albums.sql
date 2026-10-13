-- Albums named by the synthetic population's scrobbles (population.sql,
-- follows.sql, heavy.sql), for measuring album attribution (docs/social.md,
-- Albums):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/albums.sql
--
-- Run it as a superuser before compressing anything (it updates
-- scrobbles, skipping triggers) and before migration 0032, whose votes it
-- leaves to `worker albums backfill`. Deterministic. Every album of
-- follows.sql gets a deluxe edition. 85 % of accounts scrobble through a
-- client that names albums; one in ten of those plays the deluxe edition
-- of everything. Each track then belongs, as before attribution by
-- accounts, to the album its earliest scrobble named.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM albums) THEN
        RAISE EXCEPTION 'run follows.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM scrobbles WHERE album_id IS NOT NULL LIMIT 1) THEN
        RAISE EXCEPTION 'scrobbles name albums already';
    END IF;
END $$;

SELECT setseed(0.23);

BEGIN;
SET LOCAL session_replication_role = replica;

INSERT INTO albums (artist_id, title, title_normalized, image_url)
SELECT artist_id, title || ' (Deluxe)', title_normalized || ' (deluxe)', image_url
FROM albums;

CREATE TEMP TABLE synth_releases AS
SELECT t.id AS track_id, t.album_id, d.id AS deluxe_id
FROM tracks t
JOIN albums al ON al.id = t.album_id
JOIN albums d ON d.artist_id = al.artist_id AND d.title_normalized = al.title_normalized || ' (deluxe)';
ALTER TABLE synth_releases ADD PRIMARY KEY (track_id);

CREATE TEMP TABLE synth_clients AS
SELECT id AS user_id, draw < 0.85 AS names_albums, draw < 0.085 AS deluxe
FROM (SELECT id, random() AS draw FROM users) u;
ALTER TABLE synth_clients ADD PRIMARY KEY (user_id);

UPDATE scrobbles s
SET album_id = CASE WHEN c.deluxe THEN r.deluxe_id ELSE r.album_id END
FROM synth_clients c, synth_releases r
WHERE c.user_id = s.user_id AND c.names_albums AND r.track_id = s.track_id;

UPDATE tracks SET album_id = NULL;
UPDATE tracks t SET album_id = f.album_id
FROM (
    SELECT DISTINCT ON (track_id) track_id, album_id
    FROM scrobbles
    WHERE album_id IS NOT NULL
    ORDER BY track_id, played_at, id
) f
WHERE f.track_id = t.id;

UPDATE albums al SET scrobble_count = COALESCE(c.n, 0)
FROM (SELECT al2.id, n FROM albums al2
      LEFT JOIN (SELECT album_id, count(*) AS n FROM scrobbles
                 WHERE album_id IS NOT NULL GROUP BY 1) p
        ON p.album_id = al2.id) c
WHERE c.id = al.id;

COMMIT;

VACUUM ANALYZE scrobbles;
ANALYZE tracks;
ANALYZE albums;

SELECT (SELECT count(*) FROM scrobbles WHERE album_id IS NOT NULL) AS plays_naming_an_album,
       (SELECT count(*) FROM scrobbles) AS plays,
       (SELECT count(*) FROM tracks t JOIN albums al ON al.id = t.album_id
        WHERE al.title_normalized LIKE '%(deluxe)') AS tracks_on_a_deluxe_edition,
       (SELECT count(*) FROM tracks WHERE album_id IS NOT NULL) AS tracks_with_an_album;
