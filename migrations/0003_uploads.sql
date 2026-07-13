-- =============================================================
--  newfm — User-uploaded images
--  User-uploaded artwork must survive enrichment: the worker's
--  merge policy only writes image_url while image_locked is FALSE.
-- =============================================================

ALTER TABLE artists ADD COLUMN image_locked BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE albums  ADD COLUMN image_locked BOOLEAN NOT NULL DEFAULT FALSE;
