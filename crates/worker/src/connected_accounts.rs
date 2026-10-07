//! Polls connected Spotify accounts and turns their listening history into
//! scrobbles via the same `ingest_scrobble` path the `/v1/scrobble` HTTP
//! handler uses. Single file for now since there is only one real provider
//! (Deezer has no official now-playing/recently-played endpoint) — split
//! into per-provider modules if that changes.

use std::sync::Arc;

use chrono::Utc;
use db::queries::scrobble_clients::{self as clients_db, ClientIdentity, PROTOCOL_SPOTIFY};
use db::queries::{
    connected_accounts as connected_accounts_db, enrichment as enrichment_db,
    scrobbles as scrobbles_db, tracks as tracks_db,
};
use fred::interfaces::PubsubInterface;
use shared::{
    models::ConnectedAccount,
    scrobble::{self as scrobble_logic, ScrobbleInput},
    spotify::SpotifyError,
};
use sqlx::PgPool;

const BATCH_SIZE: i64 = 25;
const POLL_INTERVAL_SECS: u64 = 45;

pub struct ConnectedAccountsPoller {
    db: PgPool,
    redis: Option<fred::clients::Client>,
    http: reqwest::Client,
    spotify_client_id: Option<String>,
    spotify_client_secret: Option<String>,
    scrobble_client: tokio::sync::OnceCell<i32>,
}

impl ConnectedAccountsPoller {
    pub fn from_env(db: PgPool, redis: Option<fred::clients::Client>) -> Self {
        Self {
            db,
            redis,
            http: reqwest::Client::new(),
            spotify_client_id: std::env::var("SPOTIFY_CLIENT_ID").ok(),
            spotify_client_secret: std::env::var("SPOTIFY_CLIENT_SECRET").ok(),
            scrobble_client: tokio::sync::OnceCell::new(),
        }
    }

    /// The poller's own `scrobble_clients` row: Spotify's API, read by the
    /// server, so verified.
    async fn scrobble_client(&self) -> Option<i32> {
        let client = ClientIdentity::new(PROTOCOL_SPOTIFY, "spotify", true);
        self.scrobble_client
            .get_or_try_init(|| clients_db::resolve_client(&self.db, &client))
            .await
            .map_err(|e| {
                tracing::warn!("connected_accounts: failed to resolve scrobble client: {e}")
            })
            .ok()
            .copied()
    }

    /// Runs forever, polling active Spotify connections on a fixed
    /// interval. A no-op (returns immediately, logging once) if Spotify
    /// OAuth credentials aren't configured — lets the rest of the worker
    /// run fine in environments that haven't set this up yet.
    pub async fn run(self: Arc<Self>) {
        if self.spotify_client_id.is_none() || self.spotify_client_secret.is_none() {
            tracing::info!(
                "worker: SPOTIFY_CLIENT_ID/SPOTIFY_CLIENT_SECRET not set — connected-accounts polling disabled"
            );
            // Park instead of returning: main treats a finished task as fatal.
            return std::future::pending().await;
        }
        // Stored tokens are ciphertext; without a usable key every poll
        // would fail on decrypt. Bail loudly once instead of once per tick.
        if let Err(e) = shared::crypto::check_key() {
            tracing::error!("worker: connected-accounts polling disabled — {e}");
            return std::future::pending().await;
        }

        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS));
        loop {
            interval.tick().await;
            match connected_accounts_db::list_accounts_to_poll(&self.db, "spotify", BATCH_SIZE)
                .await
            {
                Ok(accounts) => {
                    for account in accounts {
                        if let Err(e) = self.poll_account(&account).await {
                            tracing::warn!(
                                "connected_accounts: poll failed for account {} (user {}): {e}",
                                account.id,
                                account.user_id
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("connected_accounts: failed to list accounts to poll: {e}")
                }
            }
        }
    }

    async fn poll_account(&self, account: &ConnectedAccount) -> anyhow::Result<()> {
        // Spotify's `after` is exclusive, and `history_cursor_at` holds the
        // `played_at` of the newest play we actually ingested — so a poll
        // never re-ingests a play seen on a previous tick, and never skips
        // one the endpoint hadn't caught up to yet.
        let after_ms = account.history_cursor_at.map(|t| t.timestamp_millis());

        let items =
            match shared::spotify::get_recently_played(&self.http, &account.access_token, after_ms)
                .await
            {
                Ok(items) => items,
                Err(SpotifyError::Unauthorized) => {
                    match self.retry_after_refresh(account, after_ms).await {
                        Ok(items) => items,
                        Err(e) => {
                            let _ = connected_accounts_db::mark_polled(
                                &self.db,
                                account.id,
                                Utc::now(),
                                None,
                                Some(&e.to_string()),
                            )
                            .await;
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    connected_accounts_db::mark_polled(
                        &self.db,
                        account.id,
                        Utc::now(),
                        None,
                        Some(&e.to_string()),
                    )
                    .await?;
                    return Err(e.into());
                }
            };

        // Spotify returns newest-first; walk it oldest-first so the cursor
        // only ever advances over a contiguous run of plays we're done
        // with. Anything left after a transient failure is re-fetched next
        // tick rather than being silently dropped.
        let mut cursor = None;
        for item in items.iter().rev() {
            let input = ScrobbleInput {
                track_title: item.track_title.clone(),
                artist_name: item.artist_name.clone(),
                featured_artists: item.featured_artists.clone(),
                album_title: item.album_title.clone(),
                played_at: item.played_at,
                duration_ms: Some(item.duration_ms),
                // Spotify itself already decided this play was significant
                // enough to record, so it always clears our own thresholds.
                listened_ms: Some(item.duration_ms),
                source: "spotify".into(),
                client_id: self.scrobble_client().await,
            };

            match scrobbles_db::ingest_scrobble(&self.db, account.user_id, &input).await {
                // Recorded now, or recorded already — either way this play
                // is accounted for and the cursor may move past it.
                Ok(_) | Err(scrobbles_db::IngestError::Duplicate) => {}
                // Spotify sent something we would never accept (a blank
                // artist, say). That's deterministic, so retrying it every
                // tick would wedge the cursor here forever — step over it.
                Err(e @ scrobbles_db::IngestError::Validation(_)) => tracing::warn!(
                    "connected_accounts: skipping unusable spotify play for user {}: {e}",
                    account.user_id
                ),
                // Transient. Stop advancing so the next poll retries from
                // this play onwards.
                Err(e) => {
                    tracing::warn!(
                        "connected_accounts: failed to ingest spotify scrobble for user {}: {e}",
                        account.user_id
                    );
                    break;
                }
            }
            cursor = Some(item.played_at);
        }

        connected_accounts_db::mark_polled(&self.db, account.id, Utc::now(), cursor, None).await?;

        // Best-effort: the live now-playing widget is a nice-to-have on top
        // of scrobbling, not required for it — a failure here (e.g. the
        // account was connected before `user-read-currently-playing` was
        // added to our scope and needs reconnecting) must never be treated
        // as a poll failure for the account.
        if let Err(e) = self.poll_now_playing(account).await {
            tracing::warn!(
                "connected_accounts: now-playing poll failed for user {}: {e}",
                account.user_id
            );
        }

        Ok(())
    }

    /// Fetches what the user is playing right now on Spotify and, if it's
    /// different from the currently recorded now-playing state, updates
    /// `now_playing` and republishes over the same Redis channel the
    /// `/v1/now-playing` HTTP handler uses — so the SSE-backed live widget
    /// works for Spotify the same way it does for the browser extension.
    async fn poll_now_playing(&self, account: &ConnectedAccount) -> anyhow::Result<()> {
        let Some(redis) = &self.redis else {
            return Ok(()); // no live-refresh channel configured; skip quietly
        };

        let current =
            match shared::spotify::get_currently_playing(&self.http, &account.access_token).await {
                Ok(current) => current,
                Err(SpotifyError::Unauthorized) => {
                    let refreshed = self.refresh(account).await?;
                    shared::spotify::get_currently_playing(&self.http, &refreshed.access_token)
                        .await?
                }
                Err(e) => return Err(e.into()),
            };

        let Some(current) = current else {
            return Ok(()); // nothing playing right now
        };

        // Skip the update if this is the same track already recorded as
        // playing — avoids resetting `started_at` and re-publishing over
        // SSE every 45s while the same track keeps playing.
        if let Some(existing) = scrobbles_db::get_now_playing(&self.db, account.user_id).await? {
            let same_track = scrobble_logic::normalize_name(&existing.track_title)
                == scrobble_logic::normalize_name(&current.track_title)
                && scrobble_logic::normalize_name(&existing.artist_name)
                    == scrobble_logic::normalize_name(&current.artist_name);
            if same_track {
                return Ok(());
            }
        }

        let artist = tracks_db::find_or_create_artist(&self.db, &current.artist_name).await?;
        let album_id = match &current.album_title {
            Some(title) => Some(tracks_db::find_or_create_album(&self.db, artist.id, title).await?),
            None => None,
        };
        let track = tracks_db::find_or_create_track(
            &self.db,
            artist.id,
            album_id,
            &current.track_title,
            Some(current.duration_ms),
        )
        .await?;

        let featured = scrobble_logic::normalize_featured_artists(
            &current.artist_name,
            &current.featured_artists,
        );
        let mut featured_ids = Vec::with_capacity(featured.len());
        for name in &featured {
            featured_ids.push(tracks_db::find_or_create_artist(&self.db, name).await?.id);
        }
        tracks_db::record_track_credits(&self.db, track.id, artist.id, &featured_ids).await?;

        if let Err(e) =
            enrichment_db::enqueue_for_ingest(&self.db, artist.id, album_id, track.id).await
        {
            tracing::warn!("failed to enqueue enrichment for spotify now-playing: {e}");
        }

        let remaining_ms = (current.duration_ms - current.progress_ms).max(0);
        let expires_at = Utc::now() + chrono::Duration::milliseconds(remaining_ms as i64);

        scrobbles_db::upsert_now_playing(
            &self.db,
            &scrobbles_db::UpsertNowPlaying {
                user_id: account.user_id,
                track_id: track.id,
                artist_id: artist.id,
                album_id,
                source: "spotify".into(),
                expires_at,
            },
        )
        .await?;

        if let Some(rich) = scrobbles_db::get_now_playing(&self.db, account.user_id).await? {
            let channel = format!("now_playing:{}", account.user_id);
            let payload = serde_json::to_string(&rich)?;
            let _: () = redis.publish(channel, payload).await?;
        }

        Ok(())
    }

    /// Refreshes the access token and retries the `recently-played` fetch
    /// with it. Split out from `poll_account` so both failure sources (a
    /// rejected refresh, or the retried fetch itself failing) are reported
    /// through a single `?` at the call site.
    async fn retry_after_refresh(
        &self,
        account: &ConnectedAccount,
        after_ms: Option<i64>,
    ) -> anyhow::Result<Vec<shared::spotify::RecentlyPlayedItem>> {
        let refreshed = self.refresh(account).await?;
        let items =
            shared::spotify::get_recently_played(&self.http, &refreshed.access_token, after_ms)
                .await?;
        Ok(items)
    }

    /// Refreshes an expired access token and persists it. A rejected
    /// refresh token (revoked or expired) deactivates the connection
    /// instead of retrying forever — the user has to reconnect manually.
    async fn refresh(&self, account: &ConnectedAccount) -> anyhow::Result<ConnectedAccount> {
        let refresh_token = account
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no refresh_token stored for account {}", account.id))?;

        let client_id = self.spotify_client_id.as_deref().expect("checked in run()");
        let client_secret = self
            .spotify_client_secret
            .as_deref()
            .expect("checked in run()");

        match shared::spotify::refresh_access_token(
            &self.http,
            client_id,
            client_secret,
            refresh_token,
        )
        .await
        {
            Ok(tokens) => {
                let expires_at = Utc::now() + chrono::Duration::seconds(tokens.expires_in);
                connected_accounts_db::update_tokens(
                    &self.db,
                    account.id,
                    &tokens.access_token,
                    tokens.refresh_token.as_deref(),
                    Some(expires_at),
                )
                .await?;

                Ok(ConnectedAccount {
                    access_token: tokens.access_token,
                    refresh_token: tokens
                        .refresh_token
                        .or_else(|| account.refresh_token.clone()),
                    expires_at: Some(expires_at),
                    ..account.clone()
                })
            }
            // Only a genuine revocation (401, or an `invalid_grant` body)
            // means the user has to reconnect — deactivate so a settings UI
            // can prompt for that. Any other failure (network blip, a
            // transient 5xx from Spotify) is not the account's fault, so it
            // must NOT be deactivated: leave it active and let the next
            // poll retry with the same refresh_token.
            Err(e @ shared::spotify::SpotifyError::Unauthorized) => {
                connected_accounts_db::deactivate(&self.db, account.id, &e.to_string()).await?;
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }
}
