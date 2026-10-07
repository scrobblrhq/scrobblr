-- =============================================================
--  Scrobblr — Duplicate scrobbles (classifier rules v2)
--
--  The same listen reported more than once (several scrobblers, or one
--  retrying) is labelled duplicate: never counted, like suspect, but no
--  sign of botting on its own. scrobble_flags.reason is then 'repeat'.
-- =============================================================

ALTER TYPE scrobble_status ADD VALUE 'duplicate' BEFORE 'no_data';

ALTER TABLE scrobble_classification_days ADD COLUMN duplicate INT NOT NULL DEFAULT 0;
