-- =============================================================
--  Scrobblr — Connected third-party accounts (OAuth)
--  Stores OAuth tokens for external streaming services connected to
--  a Scrobblr user. Polled by the worker to auto-scrobble listening
--  activity reported by the provider's own official API — no
--  browser extension / DOM scraping involved for these sources.
--
--  v1 supports Spotify only. Deezer's public API has no authenticated
--  "currently playing" / "recently played" endpoint as of this
--  writing (confirmed via their developer forum), so it is not
--  representable here yet; the `provider` column is free-text rather
--  than an enum so a future provider doesn't need a migration.
-- =============================================================

CREATE TABLE connected_accounts (
    id                BIGSERIAL       PRIMARY KEY,
    user_id           BIGINT          NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider          TEXT            NOT NULL,       -- 'spotify' (more may be added later)
    provider_user_id  TEXT            NOT NULL,       -- external account id
    -- Encrypted at rest (AES-256-GCM, keyed by TOKEN_ENCRYPTION_KEY) by
    -- `shared::crypto`, so a leaked dump/backup/snapshot doesn't hand out
    -- access to users' streaming accounts. Stored as `v1:<base64>` text.
    access_token      TEXT            NOT NULL,
    refresh_token     TEXT,
    token_type        TEXT            NOT NULL DEFAULT 'Bearer',
    scope             TEXT,
    expires_at        TIMESTAMPTZ,
    -- Worker bookkeeping: when we last *attempted* a poll. Purely a
    -- scheduling key (accounts are polled oldest-first) — deliberately NOT
    -- the history cursor, see below.
    last_polled_at    TIMESTAMPTZ,
    -- The `after` cursor passed to Spotify's recently-played endpoint: the
    -- `played_at` of the newest play we have actually ingested. Wall-clock
    -- time can't be used here — recently-played lags real time, so a play
    -- that happens during the poll window but lands in the response a few
    -- seconds later would fall behind a now()-based cursor and be skipped
    -- forever. Advancing only to what we really saw makes that impossible.
    history_cursor_at TIMESTAMPTZ,
    last_error        TEXT,
    -- User-toggleable pause without losing the tokens; also flipped to
    -- FALSE by the worker itself when a refresh token is permanently
    -- rejected, so a settings UI can prompt the user to reconnect.
    is_active         BOOLEAN         NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ     NOT NULL DEFAULT NOW(),

    UNIQUE (user_id, provider),                 -- one connection per provider per Scrobblr user
    UNIQUE (provider, provider_user_id)         -- the same external account can't link to 2 Scrobblr users
);

CREATE INDEX idx_connected_accounts_user    ON connected_accounts (user_id);
CREATE INDEX idx_connected_accounts_polling ON connected_accounts (provider, last_polled_at)
    WHERE is_active;

CREATE TRIGGER trg_connected_accounts_updated_at
    BEFORE UPDATE ON connected_accounts
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
