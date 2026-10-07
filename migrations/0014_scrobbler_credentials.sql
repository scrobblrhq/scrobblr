-- =============================================================
--  Scrobblr — Credentials for third-party scrobblers
--
--  What clients speaking the Last.fm, Audioscrobbler 1.2 or ListenBrainz
--  protocols hold. Kept apart from api_tokens on purpose: these
--  authenticate the compatibility endpoints only, never the native API, so
--  a key leaked from some player's config can scrobble and nothing else.
-- =============================================================

CREATE TABLE scrobbler_credentials (
    id             UUID         PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id        BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- token: made by the user to paste into a client (a ListenBrainz token,
    -- an Audioscrobbler or Last.fm password). session: a Last.fm session key
    -- a client obtained by logging in.
    kind           TEXT         NOT NULL CHECK (kind IN ('token', 'session')),
    name           TEXT         NOT NULL,
    key_hash       TEXT         NOT NULL UNIQUE,   -- sha256 of the secret
    -- md5 of a token, encrypted with TOKEN_ENCRYPTION_KEY: Audioscrobbler
    -- 1.2 and Last.fm's old authToken login prove knowledge of it.
    legacy_secret  TEXT,
    -- The Last.fm api_key a session was issued to; it works with no other.
    api_key        TEXT,
    created_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    last_used_at   TIMESTAMPTZ
);

CREATE INDEX idx_scrobbler_credentials_user ON scrobbler_credentials (user_id);

-- Last.fm's browser authorization: a client gets a token (auth.getToken,
-- or the web flow's callback), the user approves it on the web app, and
-- auth.getSession trades it, once, for a session.
CREATE TABLE scrobbler_authorizations (
    token        TEXT         PRIMARY KEY,
    api_key      TEXT         NOT NULL,
    user_id      BIGINT       REFERENCES users (id) ON DELETE CASCADE,   -- set on approval
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ  NOT NULL
);
