-- =============================================================
--  Scrobblr — Reports
--
--  Users report a bio revision, or a tag (on one entity, or anywhere);
--  reports wait in a queue for an operator's `worker moderation`
--  (docs/operations.md, Moderation). One open report per reporter and
--  target. A reporter's reports go with their account.
-- =============================================================

CREATE TABLE reports (
    id           BIGSERIAL    PRIMARY KEY,
    reporter_id  BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    revision_id  BIGINT       REFERENCES bio_revisions (id) ON DELETE CASCADE,
    tag_id       BIGINT       REFERENCES tags (id) ON DELETE CASCADE,
    -- For a tag: the entity it was seen on; NULL for the tag anywhere.
    entity_type  TEXT         CHECK (entity_type IN ('artist', 'album', 'track')),
    entity_id    BIGINT,
    reason       TEXT         NOT NULL
                 CHECK (reason IN ('spam', 'vandalism', 'offensive', 'personal_data', 'copyright', 'other')),
    note         TEXT         NOT NULL DEFAULT '',
    status       TEXT         NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'dismissed')),
    -- What the operator did, in their words.
    resolution   TEXT,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    resolved_at  TIMESTAMPTZ,

    CHECK ((revision_id IS NULL) <> (tag_id IS NULL)),
    CHECK ((entity_type IS NULL) = (entity_id IS NULL)),
    CHECK (revision_id IS NULL OR entity_type IS NULL)
);

-- One open report per reporter and target.
CREATE UNIQUE INDEX idx_reports_open ON reports
    (reporter_id, COALESCE(revision_id, 0), COALESCE(tag_id, 0), COALESCE(entity_type, ''), COALESCE(entity_id, 0))
    WHERE status = 'open';
CREATE INDEX idx_reports_queue ON reports (created_at) WHERE status = 'open';
CREATE INDEX idx_reports_revision ON reports (revision_id) WHERE revision_id IS NOT NULL;
CREATE INDEX idx_reports_tag ON reports (tag_id) WHERE tag_id IS NOT NULL;
