-- =============================================================
--  Scrobblr — Global ranking snapshots (shadow mode)
--
--  The worker sums ranking_daily over each period on a schedule and keeps
--  the top entries here, so serving a ranking reads a few hundred rows by
--  primary key. A refresh replaces a period's rows in one transaction:
--  readers see the previous snapshot until it commits, and keep it when
--  it fails. Nothing user-facing reads these tables yet.
-- =============================================================

CREATE TABLE ranking_snapshots (
    period        TEXT         NOT NULL CHECK (period IN ('week', 'month', 'year')),
    kind          TEXT         NOT NULL CHECK (kind IN ('artist', 'track')),
    -- The UTC days summed, both included.
    from_day      DATE         NOT NULL,
    to_day        DATE         NOT NULL,
    ruleset_id    INT          NOT NULL REFERENCES ranking_rulesets (id),
    -- Entities with any weight in the period; ranking_entries holds the
    -- first of them.
    ranked        INT          NOT NULL,
    -- User-days of the period still queued for weighing when it ran.
    pending_days  INT          NOT NULL,
    computed_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    took_ms       INT          NOT NULL,

    PRIMARY KEY (period, kind)
);

-- Ordered by listener credits, then weight. Raw counts (every user, every
-- scrobble) are kept beside them for comparison.
CREATE TABLE ranking_entries (
    period         TEXT    NOT NULL,
    kind           TEXT    NOT NULL,
    position       INT     NOT NULL,
    entity_id      BIGINT  NOT NULL,
    -- Thousandths, as in ranking_daily.
    listeners      BIGINT  NOT NULL,
    weight         BIGINT  NOT NULL,
    raw_listeners  INT     NOT NULL,
    raw_plays      INT     NOT NULL,

    PRIMARY KEY (period, kind, position),
    FOREIGN KEY (period, kind) REFERENCES ranking_snapshots (period, kind)
        ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
