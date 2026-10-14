-- =============================================================
--  Scrobblr — Loved tracks in Last.fm imports
--
--  An import reads the user's loved tracks (user.getLovedTracks) before
--  the history, page by page under its lease, each page committed with
--  `loved_page`. A re-import reads only loves newer than its window.
-- =============================================================

ALTER TABLE scrobble_imports
    ADD COLUMN loved_page      INT      NOT NULL DEFAULT 1,
    ADD COLUMN loved_done      BOOLEAN  NOT NULL DEFAULT FALSE,
    ADD COLUMN loved_total     BIGINT,
    ADD COLUMN loved_imported  BIGINT   NOT NULL DEFAULT 0,
    -- history_hidden | cap_reached | lastfm_error: why loves stopped short.
    ADD COLUMN loved_error     TEXT;

-- Finished imports read no loves; running ones read them next.
UPDATE scrobble_imports SET loved_done = TRUE WHERE status NOT IN ('pending', 'running');
