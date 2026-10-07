-- =============================================================
--  Scrobblr — Which client sent each scrobble
--
--  scrobbles.source is whatever the client claims. This records what the
--  server saw: the protocol a scrobble arrived by and the client as that
--  protocol identifies it (a Last.fm api_key, an Audioscrobbler client id,
--  a ListenBrainz submission_client). verified: the server checked that
--  identity (a request signed with a secret it knows, or its own poller).
--  For classification rules that weigh clients differently later.
-- =============================================================

CREATE TABLE scrobble_clients (
    id             SERIAL       PRIMARY KEY,
    protocol       TEXT         NOT NULL,   -- scrobblr | lastfm | audioscrobbler | listenbrainz | spotify
    name           TEXT         NOT NULL,
    verified       BOOLEAN      NOT NULL,
    first_seen_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    UNIQUE (protocol, name, verified)
);

-- No FK, like import_id: it would add a lookup to every insert.
ALTER TABLE scrobbles ADD COLUMN client_id INT;
