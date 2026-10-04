-- =============================================================
--  Scrobblr — Real-time aggregation for the daily aggregates
--
--  Policies never materialize the open day bucket, so with
--  materialized_only = true today's scrobbles were missing until tomorrow.
--  Real-time aggregation reads rows past the watermark (~1 day) raw.
-- =============================================================

ALTER MATERIALIZED VIEW scrobbles_daily_by_artist SET (timescaledb.materialized_only = false);
ALTER MATERIALIZED VIEW scrobbles_daily_by_track  SET (timescaledb.materialized_only = false);
ALTER MATERIALIZED VIEW user_activity_daily       SET (timescaledb.materialized_only = false);
