-- Tags and bio revisions for the synthetic population (population.sql),
-- for measuring the bio and tag routes and the tag counts (docs/social.md,
-- Bios and Tags):
--
--   psql "$SCRATCH_URL" -v users=4000 -v days=3 -f scripts/synthetic/population.sql
--   psql "$SCRATCH_URL" -f scripts/synthetic/community.sql
--   DATABASE_URL="$SCRATCH_URL" worker tags refresh
--
-- Deterministic. Tags and bios read accounts and the catalog, not plays,
-- so a few days of listening are enough. Albums are made if there are
-- none (two per artist, as follows.sql makes them). Every synthetic
-- account tags a number of entities drawn log-normally (median 60), Zipf-
-- like by popularity, 85 % artists, 10 % albums, 5 % tracks, each with 1
-- to 3 of 3,000 tags, Zipf-like too; 5 % of accounts carry a coordination
-- factor. One more account, `tagger`, holds 49,999 taggings, the cap but
-- one. The 5,000 most popular artists have an imported bio, a third of
-- them edited 1 to 20 times since; artist 1 has a history of 10,000
-- revisions and a 10,000-character text. Prints the numbers and sizes.

\set ON_ERROR_STOP on

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE username = 'synth1') THEN
        RAISE EXCEPTION 'run population.sql first';
    END IF;
    IF EXISTS (SELECT 1 FROM user_tags) THEN
        RAISE EXCEPTION 'this database has tags already';
    END IF;
END $$;

SELECT setseed(0.56);

INSERT INTO albums (artist_id, title, title_normalized)
SELECT a.id, a.name || ' ' || side, lower(a.name || ' ' || side)
FROM artists a, (VALUES ('I'), ('II')) s(side)
WHERE NOT EXISTS (SELECT 1 FROM albums)
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE ranked AS
SELECT 'artist' AS entity_type, id AS entity_id,
       row_number() OVER (ORDER BY scrobble_count DESC, id) AS rank
FROM artists
UNION ALL
SELECT 'album', id, row_number() OVER (ORDER BY artist_id, id) FROM albums
UNION ALL
SELECT 'track', id, row_number() OVER (ORDER BY scrobble_count DESC, id) FROM tracks;
CREATE INDEX ON ranked (entity_type, rank);
ANALYZE ranked;

INSERT INTO tags (name)
SELECT 'genre ' || n FROM generate_series(1, 3000) n;

-- Zipf-like draws: rank = floor(N ^ u) for u uniform.
CREATE TEMP TABLE wanted AS
SELECT u.id AS user_id, g.k,
       CASE WHEN g.r < 0.85 THEN 'artist' WHEN g.r < 0.95 THEN 'album' ELSE 'track' END
           AS entity_type,
       g.e AS draw, 1 + floor(random() * 3)::int AS tags
FROM users u
CROSS JOIN LATERAL (
    SELECT LEAST(2000, GREATEST(1, floor(exp(ln(60) + 1.1 * random_normal()))))::int AS n
) w
CROSS JOIN LATERAL (
    SELECT k, random() AS r, random() AS e FROM generate_series(1, w.n) k
) g
WHERE u.username LIKE 'synth%';

INSERT INTO user_tags (user_id, entity_type, entity_id, tag_id, source, created_at)
SELECT DISTINCT ON (w.user_id, x.entity_id, w.entity_type, t.n)
       w.user_id, w.entity_type, x.entity_id,
       (SELECT min(id) FROM tags) + t.n - 1,
       'scrobblr', NOW() - random() * INTERVAL '55 days'
FROM wanted w
JOIN LATERAL (
    SELECT entity_id FROM ranked
    WHERE entity_type = w.entity_type
      AND rank = floor(power(CASE w.entity_type WHEN 'artist' THEN 20000
                                                WHEN 'album' THEN 40000 ELSE 160000 END,
                              w.draw))::bigint
) x ON TRUE
CROSS JOIN LATERAL (
    SELECT floor(power(3000, random() * 0.999))::int + 1 AS n FROM generate_series(1, w.tags)
) t
ON CONFLICT DO NOTHING;

INSERT INTO users (username, email, password_hash, created_at)
VALUES ('tagger', 'tagger@synthetic.test', 'x', NOW() - INTERVAL '400 days');
INSERT INTO user_tags (user_id, entity_type, entity_id, tag_id, source, created_at)
SELECT u.id, 'track', t.id, (SELECT min(id) FROM tags) + (t.id % 3000),
       'scrobblr', NOW() - random() * INTERVAL '390 days'
FROM users u, (SELECT id FROM tracks ORDER BY id LIMIT 49999) t
WHERE u.username = 'tagger';

-- Some accounts flagged by the coordination detection.
INSERT INTO coordination_days (user_id, day, factor, run_id)
SELECT id, CURRENT_DATE - (id % 20)::int, (id * 37 % 1000)::int, 1
FROM users
WHERE username LIKE 'synth%' AND id % 20 = 0
ON CONFLICT DO NOTHING;

-- Bios: imports of the most popular artists, then edits by accounts.
INSERT INTO bio_revisions (entity_type, entity_id, kind, body, status, created_at)
SELECT 'artist', entity_id, 'import',
       'Imported biography of artist ' || entity_id || '. ' || repeat('Some history. ', 40),
       'published', NOW() - INTERVAL '50 days'
FROM ranked WHERE entity_type = 'artist' AND rank <= 5000;

INSERT INTO bio_revisions (entity_type, entity_id, author_id, kind, body, summary, status, created_at)
SELECT 'artist', r.entity_id, u.id, 'edit',
       'Edited biography ' || g || ' of artist ' || r.entity_id || '. ' || repeat('More history. ', 50),
       'edit ' || g, 'published', NOW() - INTERVAL '49 days' + g * INTERVAL '1 hour'
FROM ranked r
CROSS JOIN LATERAL generate_series(1, 1 + (r.entity_id * 7 % 20)::int) g
JOIN users u ON u.username = 'synth' || (1 + (r.entity_id * 31 + g * 17) % 4000)
WHERE r.entity_type = 'artist' AND r.rank <= 5000 AND r.entity_id % 3 = 0;

INSERT INTO bio_revisions (entity_type, entity_id, author_id, kind, body, summary, status, created_at)
SELECT 'artist', r.entity_id, u.id, 'edit',
       'Revision ' || g || '. ' || repeat('An edit war goes on. ', 30 + g % 400),
       'war ' || g, 'published', NOW() - INTERVAL '40 days' + g * INTERVAL '5 minutes'
FROM ranked r
CROSS JOIN generate_series(1, 10000) g
JOIN users u ON u.username = 'synth' || (1 + g % 4000)
WHERE r.entity_type = 'artist' AND r.rank = 1;

UPDATE bio_revisions SET body = left(repeat('Long text, the most a bio holds. ', 400), 10000)
WHERE id = (SELECT max(id) FROM bio_revisions);

UPDATE bio_revisions r SET parent_id = p.prev
FROM (SELECT id, lag(id) OVER (PARTITION BY entity_type, entity_id ORDER BY id) AS prev
      FROM bio_revisions) p
WHERE p.id = r.id AND p.prev IS NOT NULL;

INSERT INTO entity_bios (entity_type, entity_id, revision_id)
SELECT entity_type, entity_id, max(id) FROM bio_revisions GROUP BY 1, 2
ON CONFLICT (entity_type, entity_id) DO UPDATE SET revision_id = EXCLUDED.revision_id;
UPDATE artists a SET bio = r.body
FROM entity_bios b JOIN bio_revisions r ON r.id = b.revision_id
WHERE b.entity_type = 'artist' AND a.id = b.entity_id;

VACUUM ANALYZE tags;
VACUUM ANALYZE user_tags;
VACUUM ANALYZE bio_revisions;
VACUUM ANALYZE entity_bios;

SELECT (SELECT count(*) FROM user_tags) AS taggings,
       (SELECT count(DISTINCT (entity_type, entity_id)) FROM user_tags) AS tagged_entities,
       (SELECT max(c) FROM (SELECT count(*) AS c FROM user_tags
                            GROUP BY entity_type, entity_id) m) AS most_taggings_on_one,
       (SELECT count(*) FROM bio_revisions) AS revisions,
       pg_size_pretty(pg_total_relation_size('user_tags')) AS user_tags_size,
       pg_size_pretty(pg_total_relation_size('bio_revisions')) AS revisions_size;
