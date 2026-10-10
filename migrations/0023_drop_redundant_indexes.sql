-- =============================================================
--  Scrobblr — Indexes no query needs
--
--  idx_scrobbles_user_artist: nothing filters scrobbles by user and
--  artist; idx_scrobbles_user_time serves every per-user read and
--  idx_scrobbles_artist_global the per-artist one.
--
--  The (artist_id, day) and (track_id, day) indexes TimescaleDB created
--  on the daily aggregates: the covering indexes of 0015 serve the same
--  lookups, and no query filters those columns by day.
-- =============================================================

DROP INDEX idx_scrobbles_user_artist;

DO $$
DECLARE
    idx regclass;
BEGIN
    FOR idx IN
        SELECT i.indexrelid::regclass
        FROM timescaledb_information.continuous_aggregates ca
        JOIN pg_index i ON i.indrelid = format('%I.%I', ca.materialization_hypertable_schema,
                                               ca.materialization_hypertable_name)::regclass
        WHERE ca.view_name IN ('scrobbles_daily_by_artist', 'scrobbles_daily_by_track')
          AND (SELECT array_agg(a.attname::text ORDER BY k.n)
               FROM unnest(i.indkey) WITH ORDINALITY AS k(attnum, n)
               JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum)
              IN ('{artist_id,day}', '{track_id,day}')
    LOOP
        EXECUTE format('DROP INDEX %s', idx);
    END LOOP;
END
$$;
