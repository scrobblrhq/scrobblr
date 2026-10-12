-- =============================================================
--  Scrobblr — A minimum of listeners for catalog counts
--
--  A track's or artist's play count, and an artist's top tracks, were
--  served for any entity, so on a small instance a count could be one
--  person's listening. They are now served only for entities with at
--  least RANKING_MIN_LISTENERS distinct listeners, like the rankings.
-- =============================================================

-- (entity, user) order lets a count of listeners skip from user to user.
-- They replace 0015's covering indexes and serve the same sections.
CREATE INDEX idx_daily_by_artist_listener ON scrobbles_daily_by_artist (artist_id, user_id)
    INCLUDE (play_count);
CREATE INDEX idx_daily_by_track_listener  ON scrobbles_daily_by_track (track_id, user_id)
    INCLUDE (play_count);

-- The aggregates' own views can't be scanned in user order across chunks,
-- so these read the materializations: a new listener counts from the next
-- refresh.
DO $$
DECLARE
    artist_rel text;
    track_rel  text;
BEGIN
    SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name)
    INTO artist_rel
    FROM timescaledb_information.continuous_aggregates
    WHERE view_name = 'scrobbles_daily_by_artist';
    SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name)
    INTO track_rel
    FROM timescaledb_information.continuous_aggregates
    WHERE view_name = 'scrobbles_daily_by_track';

    EXECUTE format('DROP INDEX %I.idx_daily_by_artist_artist',
                   split_part(artist_rel, '.', 1));
    EXECUTE format('DROP INDEX %I.idx_daily_by_track_track',
                   split_part(track_rel, '.', 1));

    EXECUTE format('CREATE VIEW artist_listener_rows AS SELECT artist_id, user_id FROM %s', artist_rel);
    EXECUTE format('CREATE VIEW track_listener_rows AS SELECT track_id, user_id FROM %s', track_rel);
END
$$;

-- Whether at least k distinct users, private ones included, played it.
CREATE FUNCTION artist_has_listeners(p_artist_id BIGINT, k INT) RETURNS BOOLEAN
LANGUAGE sql STABLE AS $$
    WITH RECURSIVE listener(user_id, n) AS (
        (SELECT user_id, 1 FROM artist_listener_rows
         WHERE artist_id = p_artist_id ORDER BY user_id LIMIT 1)
        UNION ALL
        SELECT (SELECT r.user_id FROM artist_listener_rows r
                WHERE r.artist_id = p_artist_id AND r.user_id > l.user_id
                ORDER BY r.user_id LIMIT 1),
               l.n + 1
        FROM listener l
        WHERE l.n < k AND l.user_id IS NOT NULL
    )
    SELECT count(user_id) >= k FROM listener
$$;

CREATE FUNCTION track_has_listeners(p_track_id BIGINT, k INT) RETURNS BOOLEAN
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
    SELECT count(user_id) >= k FROM listener
$$;
