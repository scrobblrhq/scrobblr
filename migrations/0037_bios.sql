-- =============================================================
--  Scrobblr — Community bios
--
--  Artists' and albums' bios are edited like a wiki (docs/social.md,
--  Bios): every edit is a revision, kept for good and shown with its
--  author, time and summary; `entity_bios` points at the current one.
--  `artists.bio` mirrors the current text, so everything serving an
--  artist serves it; enrichment no longer writes it directly. A bio
--  Last.fm gave becomes an `import` revision, and once a person has
--  edited a bio, Last.fm's is never written over it.
--
--  Revisions are community content: an erased account's published
--  revisions stay, without their author (`ON DELETE SET NULL`); its held
--  and rejected ones, never published, go with it (docs/privacy.md).
-- =============================================================

-- What the duplicate-text check compares: case and runs of whitespace
-- don't make a copy new.
CREATE FUNCTION bio_text_hash(body TEXT) RETURNS BYTEA
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN decode(md5(lower(regexp_replace(body, '\s+', ' ', 'g'))), 'hex');

CREATE TABLE bio_revisions (
    id           BIGSERIAL    PRIMARY KEY,
    entity_type  TEXT         NOT NULL CHECK (entity_type IN ('artist', 'album')),
    entity_id    BIGINT       NOT NULL,
    -- The current revision when it was written; NULL for the first.
    parent_id    BIGINT       REFERENCES bio_revisions (id),
    -- NULL for imports and moderators' revisions, and after an erase.
    author_id    BIGINT       REFERENCES users (id) ON DELETE SET NULL,
    kind         TEXT         NOT NULL CHECK (kind IN ('edit', 'revert', 'import', 'moderation')),
    -- The revision a revert (or a moderator's revert) restored.
    reverted_to  BIGINT       REFERENCES bio_revisions (id),
    body         TEXT         NOT NULL,
    summary      TEXT         NOT NULL DEFAULT '',
    status       TEXT         NOT NULL CHECK (status IN ('published', 'held', 'rejected')),
    -- Why an edit was held for review: repeated_link, link_across_bios,
    -- duplicate_text.
    held_reason  TEXT,
    -- Hosts of the links it added to its parent's text.
    added_hosts  TEXT[]       NOT NULL DEFAULT '{}',
    body_hash    BYTEA        NOT NULL GENERATED ALWAYS AS (bio_text_hash(body)) STORED,
    -- A moderator hid its text (and summary) from everyone.
    hidden_at    TIMESTAMPTZ,
    reviewed_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- History, newest first.
CREATE INDEX idx_bio_revisions_entity ON bio_revisions (entity_type, entity_id, id DESC);
-- The daily caps, the export and the erase.
CREATE INDEX idx_bio_revisions_author ON bio_revisions (author_id, created_at DESC);
CREATE INDEX idx_bio_revisions_hash ON bio_revisions (body_hash);
CREATE INDEX idx_bio_revisions_hosts ON bio_revisions USING gin (added_hosts)
    WHERE added_hosts <> '{}';
CREATE INDEX idx_bio_revisions_held ON bio_revisions (created_at) WHERE status = 'held';

CREATE TABLE entity_bios (
    entity_type  TEXT         NOT NULL CHECK (entity_type IN ('artist', 'album')),
    entity_id    BIGINT       NOT NULL,
    -- The current text; NULL when there is none (every revision hidden).
    revision_id  BIGINT       REFERENCES bio_revisions (id),
    -- Set by a moderator: no edits or reverts until unlocked.
    locked_at    TIMESTAMPTZ,
    lock_reason  TEXT,
    updated_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (entity_type, entity_id)
);

-- An account a moderator stopped from editing bios and tagging, until
-- `until` (NULL: until lifted). Its tags weigh nothing meanwhile.
CREATE TABLE editing_suspensions (
    user_id     BIGINT       PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    until       TIMESTAMPTZ,
    reason      TEXT         NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- The bios enrichment fetched from Last.fm are their first revisions.
WITH imported AS (
    INSERT INTO bio_revisions (entity_type, entity_id, kind, body, status)
    SELECT 'artist', id, 'import', btrim(bio), 'published'
    FROM artists
    WHERE btrim(COALESCE(bio, '')) <> ''
    ORDER BY id
    RETURNING id, entity_id
)
INSERT INTO entity_bios (entity_type, entity_id, revision_id)
SELECT 'artist', entity_id, id FROM imported;

UPDATE artists SET bio = NULLIF(btrim(bio), '') WHERE bio IS DISTINCT FROM NULLIF(btrim(bio), '');
