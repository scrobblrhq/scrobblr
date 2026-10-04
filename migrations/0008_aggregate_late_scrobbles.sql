-- no-transaction
-- =============================================================
--  Scrobblr — Continuous aggregates pick up late scrobbles
--
--  With `start_offset => '3 days'`, scrobbles older than 3 days at insert
--  time (late submissions, history imports) never reached the aggregates.
--  NULL covers all history; refreshes only recompute invalidated buckets.
--
--  The CALLs can't run inside a transaction, hence `no-transaction`
--  (the runner then executes each statement on its own); `if_exists`
--  keeps a re-run after a partial failure safe.
-- =============================================================

SELECT remove_continuous_aggregate_policy('scrobbles_daily_by_artist', if_exists => true);
SELECT add_continuous_aggregate_policy('scrobbles_daily_by_artist',
                                       start_offset      => NULL,
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );

SELECT remove_continuous_aggregate_policy('scrobbles_daily_by_track', if_exists => true);
SELECT add_continuous_aggregate_policy('scrobbles_daily_by_track',
                                       start_offset      => NULL,
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );

SELECT remove_continuous_aggregate_policy('user_activity_daily', if_exists => true);
SELECT add_continuous_aggregate_policy('user_activity_daily',
                                       start_offset      => NULL,
                                       end_offset        => INTERVAL '1 hour',
                                       schedule_interval => INTERVAL '1 hour'
       );

-- Backfill what was missed. Stops before today's open bucket: materializing
-- it would leave today's later scrobbles out until tomorrow.
CALL refresh_continuous_aggregate('scrobbles_daily_by_artist', NULL, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('scrobbles_daily_by_track',  NULL, time_bucket('1 day', NOW()));
CALL refresh_continuous_aggregate('user_activity_daily',       NULL, time_bucket('1 day', NOW()));
