-- =============================================================
--  Scrobblr — Community contributions
--  Image candidates with voting (last.fm-style: uploads add a
--  candidate rather than replacing the image; the most-liked
--  candidate becomes the displayed image), and comments on
--  catalog entities.
-- =============================================================

-- One uploaded image per row. The entity's displayed image_url is set to
-- the winning candidate's url once it clears the vote threshold; the
-- upload itself never overwrites the current image directly.
CREATE TABLE image_candidates (
    id           BIGSERIAL       PRIMARY KEY,
    entity_type  TEXT            NOT NULL CHECK (entity_type IN ('artist', 'album')),
    entity_id    BIGINT          NOT NULL,
    url          TEXT            NOT NULL,
    uploaded_by  BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Denormalized like count, kept in sync on vote/unvote so candidates
    -- can be ordered without aggregating the votes table.
    vote_count   INT             NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ     NOT NULL DEFAULT NOW()
);

-- Serves "candidates for an entity, most-liked first".
CREATE INDEX idx_image_candidates_entity
    ON image_candidates (entity_type, entity_id, vote_count DESC);

-- One like per user per candidate.
CREATE TABLE image_candidate_votes (
    candidate_id BIGINT          NOT NULL REFERENCES image_candidates (id) ON DELETE CASCADE,
    user_id      BIGINT          NOT NULL REFERENCES users (id)            ON DELETE CASCADE,
    created_at   TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    PRIMARY KEY (candidate_id, user_id)
);


-- User comments on a catalog entity (artist or track).
CREATE TABLE comments (
    id           BIGSERIAL       PRIMARY KEY,
    entity_type  TEXT            NOT NULL CHECK (entity_type IN ('artist', 'track')),
    entity_id    BIGINT          NOT NULL,
    user_id      BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    body         TEXT            NOT NULL,
    created_at   TIMESTAMPTZ     NOT NULL DEFAULT NOW()
);

-- Serves "comments for an entity, newest first".
CREATE INDEX idx_comments_entity
    ON comments (entity_type, entity_id, created_at DESC);
