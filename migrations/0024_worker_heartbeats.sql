-- One row per worker loop, written as the loop runs (at most every 30 s,
-- or when it starts or stops failing). `worker status`, /health/worker and
-- /metrics read it to tell a stalled or failing loop apart.
CREATE TABLE worker_heartbeats (
    loop_name      TEXT         PRIMARY KEY,
    -- How long the loop may take between two runs; it waits longer while
    -- it backs off.
    interval_secs  INTEGER      NOT NULL CHECK (interval_secs > 0),
    -- When the worker that runs it last started.
    started_at     TIMESTAMPTZ  NOT NULL,
    last_run_at    TIMESTAMPTZ,
    last_ok_at     TIMESTAMPTZ,
    last_error_at  TIMESTAMPTZ,
    last_error     TEXT
);
