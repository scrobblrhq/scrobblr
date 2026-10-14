-- =============================================================
--  Scrobblr — ListenBrainz feedback and recording MBIDs
--
--  Loves can come from the ListenBrainz-compatible feedback API. A
--  recording MBID resolves to the track MusicBrainz enrichment matched it
--  to (`tracks.mbid`, unique), else to a track a client named it for and
--  enrichment hasn't decided yet (`mbid_hint`), indexed here.
-- =============================================================

ALTER TABLE loved_tracks DROP CONSTRAINT loved_tracks_source_check;
ALTER TABLE loved_tracks ADD CONSTRAINT loved_tracks_source_check
    CHECK (source IN ('scrobblr', 'lastfm', 'audioscrobbler', 'listenbrainz', 'lastfm_import'));

CREATE INDEX idx_tracks_mbid_hint ON tracks (mbid_hint)
    WHERE mbid IS NULL AND mbid_hint IS NOT NULL;
