-- =============================================================
--  Scrobblr — Albums, with the catalog-count minimum
--
--  An album is played through its tracks: a track belongs to the album
--  the catalog filed it under (tracks.album_id, the first one a scrobble
--  named), and its plays count for that album, in charts and in the
--  album's count. Like a track's or an artist's, that count is served only
--  when RANKING_MIN_LISTENERS distinct users played the album's tracks.
-- =============================================================

-- Up to k of a track's distinct listeners, skipping from user to user as
-- track_has_listeners does.
CREATE FUNCTION track_listener_sample(p_track_id BIGINT, k INT) RETURNS SETOF BIGINT
LANGUAGE sql STABLE AS $$
    WITH RECURSIVE listener(user_id, n) AS (
        (SELECT user_id, 1 FROM track_listener_rows
         WHERE track_id = p_track_id ORDER BY user_id LIMIT 1)
        UNION ALL
        SELECT (SELECT r.user_id FROM track_listener_rows r
                WHERE r.track_id = p_track_id AND r.user_id > l.user_id
                ORDER BY r.user_id LIMIT 1),
               l.n + 1
        FROM listener l
        WHERE l.n < k AND l.user_id IS NOT NULL
    )
    SELECT user_id FROM listener WHERE user_id IS NOT NULL
$$;

-- Whether at least k distinct users, private ones included, played its
-- tracks: k probes per track at most.
CREATE FUNCTION album_has_listeners(p_album_id BIGINT, k INT) RETURNS BOOLEAN
LANGUAGE sql STABLE AS $$
    SELECT count(DISTINCT l.user_id) >= k
    FROM tracks t
    CROSS JOIN LATERAL track_listener_sample(t.id, k) AS l(user_id)
    WHERE t.album_id = p_album_id
$$;
