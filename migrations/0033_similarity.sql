-- =============================================================
--  Scrobblr — Artist similarity, recommendations, taste neighbours
--
--  The worker computes them from the ranking weights and coordination
--  factors (shared::similarity) once a day and replaces the stored
--  results in one transaction, so readers see the previous ones until it
--  commits and keep them when it fails. Results name no account but the
--  one they are for, and taste neighbours who chose to be listed.
-- =============================================================

-- listed: counted anonymously and shown as a taste neighbour while the
-- profile is public; anonymous: counted, never shown; off: not used.
ALTER TABLE users ADD COLUMN similarity TEXT NOT NULL DEFAULT 'anonymous'
    CHECK (similarity IN ('listed', 'anonymous', 'off'));
ALTER TABLE users ADD COLUMN similarity_changed_at TIMESTAMPTZ;

CREATE TABLE similarity_runs (
    id                   SERIAL       PRIMARY KEY,
    fingerprint          TEXT         NOT NULL,
    -- The UTC days of weights read, both included.
    window_from          DATE         NOT NULL,
    window_to            DATE         NOT NULL,
    coordination_run_id  INT,
    -- Accounts with some credit, listed ones among them, and what came of
    -- it.
    accounts             INT          NOT NULL,
    candidates           INT          NOT NULL,
    artists              INT          NOT NULL,
    neighbours           INT          NOT NULL,
    recommended_users    INT          NOT NULL,
    neighboured_users    INT          NOT NULL,
    -- When the next run is due; readers call the results stale an hour
    -- after it.
    refresh_secs         INT          NOT NULL,
    -- An erased account counted in them: recompute now.
    invalidated          BOOLEAN      NOT NULL DEFAULT FALSE,
    started_at           TIMESTAMPTZ  NOT NULL,
    took_ms              INT          NOT NULL,
    finished_at          TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Catalog rows are never deleted, so the artist references need no index
-- of their own.
CREATE TABLE artist_neighbours (
    artist_id     BIGINT    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    position      SMALLINT  NOT NULL,
    neighbour_id  BIGINT    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    -- Thousandths.
    score         INT       NOT NULL,
    -- Accounts listening to both.
    listeners     INT       NOT NULL,

    PRIMARY KEY (artist_id, position)
);

-- A user's list in one row, best first: always read whole, and written
-- 100 times faster than a row per entry, whose foreign keys cost ~25 µs
-- each. For each artist, the user's artist that voted most for it and how
-- many did.
CREATE TABLE user_recommendations (
    user_id             BIGINT    PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    artist_ids          BIGINT[]  NOT NULL,
    scores              INT[]     NOT NULL,
    because_artist_ids  BIGINT[]  NOT NULL,
    because_counts      INT[]     NOT NULL
);

-- Accounts listed when the run was made; readers check they still are,
-- and are visible.
CREATE TABLE user_neighbours (
    user_id            BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    position           SMALLINT  NOT NULL,
    neighbour_user_id  BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    score              INT       NOT NULL,
    shared_artists     INT       NOT NULL,
    top_shared         BIGINT[]  NOT NULL,

    PRIMARY KEY (user_id, position)
);

-- For erasing, blocking and opting out.
CREATE INDEX idx_user_neighbours_neighbour ON user_neighbours (neighbour_user_id);

-- Recommendations the user skipped: hidden until `until`.
CREATE TABLE recommendation_skips (
    user_id     BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    artist_id   BIGINT       NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
    skipped_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    until       TIMESTAMPTZ  NOT NULL,

    PRIMARY KEY (user_id, artist_id)
);

CREATE INDEX idx_recommendation_skips_until ON recommendation_skips (until);
