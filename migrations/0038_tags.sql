-- =============================================================
--  Scrobblr — Tags
--
--  Users tag artists, albums and tracks (docs/social.md, Tags). A user's
--  tags follow the profile's visibility. What everyone sees is
--  `tag_counts`, which the worker keeps: per entity and tag, the accounts
--  that put it there and their weight, the rankings' kind (an account a
--  week old weighs its lowest coordination factor of the last 28 days;
--  newer, suspended or leaving accounts nothing). Tags feed nothing else.
-- =============================================================

CREATE TABLE tags (
    id          BIGSERIAL    PRIMARY KEY,
    -- Normalized (shared::community::tag_name): as shown.
    name        TEXT         NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE TABLE user_tags (
    id           BIGSERIAL    PRIMARY KEY,
    user_id      BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    entity_type  TEXT         NOT NULL CHECK (entity_type IN ('artist', 'album', 'track')),
    entity_id    BIGINT       NOT NULL,
    tag_id       BIGINT       NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    -- scrobblr | lastfm (the Last.fm-compatible API, with its client)
    source       TEXT         NOT NULL CHECK (source IN ('scrobblr', 'lastfm')),
    client_id    INT          REFERENCES scrobble_clients (id),
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    UNIQUE (user_id, entity_type, entity_id, tag_id)
);

-- What the worker counts per entity, and the caps per entity.
CREATE INDEX idx_user_tags_entity ON user_tags (entity_type, entity_id, tag_id);
-- A user's list, newest first.
CREATE INDEX idx_user_tags_recent ON user_tags (user_id, created_at DESC, id DESC);
-- The cascade from tags, and the blocklist.
CREATE INDEX idx_user_tags_tag ON user_tags (tag_id);

CREATE TABLE tag_counts (
    entity_type  TEXT         NOT NULL,
    entity_id    BIGINT       NOT NULL,
    tag_id       BIGINT       NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    -- Every account that put the tag there.
    taggers      INT          NOT NULL,
    -- Their weight, in thousandths: served from RANKING_MIN_LISTENERS whole.
    weight       BIGINT       NOT NULL,
    computed_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (entity_type, entity_id, tag_id)
);

-- An entity's top tags, and a tag's top entities.
CREATE INDEX idx_tag_counts_entity ON tag_counts (entity_type, entity_id, weight DESC, tag_id);
CREATE INDEX idx_tag_counts_tag ON tag_counts (tag_id, entity_type, weight DESC, entity_id);

-- Entities whose counts the worker recomputes, after a short settle so a
-- burst of tags costs one pass.
CREATE TABLE tag_queue (
    entity_type  TEXT         NOT NULL,
    entity_id    BIGINT       NOT NULL,
    due_at       TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (entity_type, entity_id)
);

CREATE INDEX idx_tag_queue_due ON tag_queue (due_at);

-- Terms an operator refuses in tags (`worker moderation block-tag`),
-- normalized like tags; one blocks every tag holding its words in a row.
CREATE TABLE tag_blocklist (
    term        TEXT         PRIMARY KEY,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
