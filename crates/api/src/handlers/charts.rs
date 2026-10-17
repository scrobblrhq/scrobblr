//! Charts over a period or a range of UTC days (`db::queries::charts`): a
//! user's own top artists, tracks and albums, and those of the accounts a
//! user follows, weighed as the global rankings are.

use aide::transform::{TransformOperation, TransformResponse};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, NaiveDate, Utc};
use fred::{interfaces::KeysInterface, types::Expiration};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{UsernamePath, caching};
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::charts::{self as charts_db, CommunityKind};
use db::queries::{rankings as rankings_db, users as users_db};
use shared::charts::{self, ChartPeriod, ChartRange, RangeQuery, RangeRules};
use shared::models::{
    AlbumChart, ArtistChart, ChartInfo, CommunityAlbumChart, CommunityArtistChart,
    CommunityChartInfo, CommunityTrackChart, RankedAlbum, RankedArtist, RankedTrack,
    ScrobbleBreakdown, TrackChart,
};
use shared::ranking::{FULL, whole};
use std::collections::HashMap;

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

/// Per viewer: a private profile's chart is its owner's alone, and any
/// profile can turn private.
const PERSONAL_CACHE_CONTROL: &str = "private, max-age=60";

/// How long a computed page is kept in Redis under its version (the
/// version itself changes with the user's next play, and every hour): a
/// long history's all-time chart takes a few hundred milliseconds.
const PERSONAL_CACHE_SECS: i64 = 900;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChartQuery {
    /// `7d` (the default), `30d`, `90d`, `12m` (365 days) or `all`: UTC days
    /// ending today, today included. Not with `from` and `to`.
    #[schemars(with = "Option<ChartPeriod>")]
    pub period: Option<String>,
    /// The first UTC day of a custom range (`YYYY-MM-DD`), with `to`.
    #[schemars(with = "Option<NaiveDate>")]
    pub from: Option<String>,
    /// The last UTC day of a custom range, included, with `from`: not after
    /// today, and 366 days at most from `from`.
    #[schemars(with = "Option<NaiveDate>")]
    pub to: Option<String>,
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// Entries to skip, 0 by default: `offset=50` starts at the 51st.
    pub offset: Option<i64>,
}

impl ChartQuery {
    fn range(&self, rules: RangeRules) -> ApiResult<ChartRange> {
        charts::resolve(
            RangeQuery {
                period: self.period.as_deref(),
                from: self.from.as_deref(),
                to: self.to.as_deref(),
            },
            Utc::now().date_naive(),
            rules,
        )
        .map_err(|e| AppError::BadRequest(e.to_string()))
    }

    fn page(&self) -> ApiResult<(i64, i64)> {
        let offset = self.offset.unwrap_or(0);
        if offset < 0 {
            return Err(AppError::BadRequest("offset can't be negative".into()));
        }
        Ok((
            super::page_limit(self.limit, DEFAULT_LIMIT, MAX_LIMIT),
            offset,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Artists,
    Tracks,
    Albums,
}

/// GET /v1/user/:username/charts/artists
pub async fn user_artists(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Response> {
    personal(&state, &username, &q, &headers, auth_user, Kind::Artists).await
}

pub fn _user_artists_doc(op: TransformOperation) -> TransformOperation {
    personal_doc(op, "User's artist chart", "artists").response_with::<200, Json<ArtistChart>, _>(
        |r| personal_headers(r).description("A page of the chart"),
    )
}

/// GET /v1/user/:username/charts/tracks
pub async fn user_tracks(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Response> {
    personal(&state, &username, &q, &headers, auth_user, Kind::Tracks).await
}

pub fn _user_tracks_doc(op: TransformOperation) -> TransformOperation {
    personal_doc(op, "User's track chart", "tracks").response_with::<200, Json<TrackChart>, _>(
        |r| personal_headers(r).description("A page of the chart"),
    )
}

/// GET /v1/user/:username/charts/albums
pub async fn user_albums(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Response> {
    personal(&state, &username, &q, &headers, auth_user, Kind::Albums).await
}

pub fn _user_albums_doc(op: TransformOperation) -> TransformOperation {
    personal_doc(op, "User's album chart", "albums").response_with::<200, Json<AlbumChart>, _>(
        |r| personal_headers(r).description("A page of the chart"),
    )
}

async fn personal(
    state: &AppState,
    username: &str,
    q: &ChartQuery,
    headers: &HeaderMap,
    auth_user: Option<Extension<AuthUser>>,
    kind: Kind,
) -> ApiResult<Response> {
    let range = q.range(RangeRules {
        default: ChartPeriod::Days7,
        earliest: None,
    })?;
    let (limit, offset) = q.page()?;
    let user = users_db::find_by_username(&state.db, username)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(state, viewer_id, &user).await?;

    let breakdown = users_db::scrobble_breakdown(&state.db, user.id).await?;
    let etag = personal_etag(&breakdown);
    if let Some(tags) = headers.get(header::IF_NONE_MATCH)
        && caching::matches(tags, &etag)
    {
        return Ok(with_personal_caching(
            StatusCode::NOT_MODIFIED.into_response(),
            &etag,
        ));
    }

    // The page as computed for this version of the user's data: the same
    // for every viewer allowed to see it.
    let key = format!(
        "chart:v1:{}:{kind:?}:{:?}:{}:{}:{limit}:{offset}:{etag}",
        user.id,
        range.period,
        range.from.map_or_else(String::new, |d| d.to_string()),
        range.to,
    );
    match state.redis.get::<Option<String>, _>(&key).await {
        Ok(Some(body)) => return Ok(with_personal_caching(json(body), &etag)),
        Ok(None) => {}
        Err(e) => tracing::warn!("chart cache unavailable: {e}"),
    }

    let info = |total| ChartInfo {
        period: range.period,
        from: range.from,
        to: range.to,
        total,
        offset,
        limit,
    };
    let db = &state.db;
    let body = match kind {
        Kind::Artists => {
            let page = charts_db::user_artists(db, user.id, &range, limit, offset).await?;
            serde_json::to_string(&ArtistChart {
                info: info(page.total),
                entries: page.entries,
            })
        }
        Kind::Tracks => {
            let page = charts_db::user_tracks(db, user.id, &range, limit, offset).await?;
            serde_json::to_string(&TrackChart {
                info: info(page.total),
                entries: page.entries,
            })
        }
        Kind::Albums => {
            let page = charts_db::user_albums(db, user.id, &range, limit, offset).await?;
            serde_json::to_string(&AlbumChart {
                info: info(page.total),
                entries: page.entries,
            })
        }
    }
    .map_err(|e| AppError::Internal(e.into()))?;
    let stored = state
        .redis
        .set::<(), _, _>(
            &key,
            body.as_str(),
            Some(Expiration::EX(PERSONAL_CACHE_SECS)),
            None,
            false,
        )
        .await;
    if let Err(e) = stored {
        tracing::warn!("chart cache unavailable: {e}");
    }
    Ok(with_personal_caching(json(body), &etag))
}

/// A JSON body already serialized.
fn json(body: String) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

/// Weak. A chart changes when the user's scrobbles or their labels do,
/// which the breakdown follows, and when a period's days move. Within the
/// hour, a late play reaching an old day's aggregate at its refresh, or a
/// catalog name or image, may wait for the next tag.
fn personal_etag(b: &ScrobbleBreakdown) -> String {
    format!(
        "W/\"pc1-{}-{}-{}-{}-{}-{}-{}\"",
        b.verified,
        b.unverified,
        b.suspect,
        b.no_data,
        b.pending,
        b.duplicates,
        Utc::now().format("%Y%m%d%H")
    )
}

fn with_personal_caching(mut response: Response, etag: &str) -> Response {
    let headers = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PERSONAL_CACHE_CONTROL),
    );
    headers.append(header::VARY, HeaderValue::from_static("Authorization"));
    response
}

fn personal_headers<T>(r: TransformResponse<'_, T>) -> TransformResponse<'_, T> {
    caching::headers(
        r,
        "The chart's version, for `If-None-Match`",
        PERSONAL_CACHE_CONTROL,
    )
}

fn personal_doc<'t>(
    op: TransformOperation<'t>,
    summary: &str,
    things: &str,
) -> TransformOperation<'t> {
    op.summary(summary)
        .description(&format!(
            "The {things} a user played most over a period (`7d` by default, `30d`, `90d`, `12m` \
             or `all`: UTC days ending today, today included) or a custom range of UTC days \
             (`from` and `to`, both included, 366 days at most, not after today), most played \
             first, ties by id. Plays are the profile's: suspect ones included and counted in \
             `unverified_count` with those of tracks without a known length, duplicates (the \
             same listen reported again) left out; today's count as they arrive, and a play \
             counts as a play until it is classified. An album's plays are its tracks' (each \
             track belongs to the album the catalog files it under).\n\n\
             Paged by `limit` (50 by default, 200 at most) and `offset`; `total` is how many \
             {things} the chart has, and a page past them is empty. Pages carry a weak `ETag` \
             and `Cache-Control: {PERSONAL_CACHE_CONTROL}`: send the tag back in \
             `If-None-Match` for a 304 while the chart hasn't changed. The server keeps a page \
             it computed until the user's next play or the hour's end, so a late play reaching \
             an old day, or a new name or image, can take up to an hour to show. Private \
             profiles return 403 to everyone but their owner."
        ))
        .tag("Charts")
        .with(|op| {
            caching::if_none_match(
                op,
                "The `ETag` of a page fetched before: 304 if the chart hasn't changed since",
            )
        })
        .response_with::<304, (), _>(|r| {
            personal_headers(r).description("Not modified since the `ETag` in `If-None-Match`")
        })
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("Unknown period, a period with a range, an invalid or too long range, or a negative offset")
        })
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// Positions a community chart keeps, as the global snapshots do.
pub const COMMUNITY_POSITIONS: i64 = 1000;

/// The viewer's own: the accounts they follow.
const COMMUNITY_CACHE_CONTROL: &str = "private, max-age=60";

/// GET /v1/user/me/community/artists
pub async fn community_artists(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    community(&state, auth_user.id, &q, &headers, Kind::Artists).await
}

pub fn _community_artists_doc(op: TransformOperation) -> TransformOperation {
    community_doc(op, "Community artist chart", "artists")
        .response_with::<200, Json<CommunityArtistChart>, _>(|r| {
            community_headers(r).description("A page of the chart")
        })
}

/// GET /v1/user/me/community/tracks
pub async fn community_tracks(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    community(&state, auth_user.id, &q, &headers, Kind::Tracks).await
}

pub fn _community_tracks_doc(op: TransformOperation) -> TransformOperation {
    community_doc(op, "Community track chart", "tracks")
        .response_with::<200, Json<CommunityTrackChart>, _>(|r| {
            community_headers(r).description("A page of the chart")
        })
}

/// GET /v1/user/me/community/albums
pub async fn community_albums(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    community(&state, auth_user.id, &q, &headers, Kind::Albums).await
}

pub fn _community_albums_doc(op: TransformOperation) -> TransformOperation {
    community_doc(op, "Community album chart", "albums")
        .response_with::<200, Json<CommunityAlbumChart>, _>(|r| {
            community_headers(r).description("A page of the chart")
        })
}

/// How long a computed community ranking is reused: paging through it, or
/// coming back to it, then costs a Redis read.
const COMMUNITY_CACHE_SECS: i64 = 300;

/// A community ranking as computed, before names.
#[derive(Debug, Serialize, Deserialize)]
struct Computed {
    computed_at: DateTime<Utc>,
    entries: Vec<charts_db::Ranked>,
}

async fn community(
    state: &AppState,
    viewer_id: i64,
    q: &ChartQuery,
    headers: &HeaderMap,
    kind: Kind,
) -> ApiResult<Response> {
    let today = Utc::now().date_naive();
    let range = q.range(RangeRules {
        default: ChartPeriod::Days7,
        earliest: Some(rankings_db::retained_from(today)),
    })?;
    let (limit, offset) = q.page()?;
    let (Some(from), Some(days)) = (range.from, range.days()) else {
        return Err(AppError::BadRequest(
            "this chart has no `all` period".into(),
        ));
    };
    let max_accounts = charts::community_accounts(days, super::users::MAX_FOLLOWING);
    let (following, accounts) =
        charts_db::community_accounts(&state.db, viewer_id, max_accounts).await?;
    let min_listeners = i64::from(state.rankings.min_listeners);
    let kind = match kind {
        Kind::Artists => CommunityKind::Artists,
        Kind::Tracks => CommunityKind::Tracks,
        Kind::Albums => CommunityKind::Albums,
    };
    let key = community_key(kind, from, range.to, min_listeners, &accounts);
    let ranking =
        community_ranking(state, &key, kind, &accounts, from, range.to, min_listeners).await?;
    let etag = format!(
        "W/\"cc1-{}-{}\"",
        &key[key.len() - 16..],
        ranking.computed_at.timestamp_micros()
    );
    if let Some(tags) = headers.get(header::IF_NONE_MATCH)
        && caching::matches(tags, &etag)
    {
        return Ok(with_community_caching(
            StatusCode::NOT_MODIFIED.into_response(),
            &etag,
        ));
    }

    let ranked = ranking.entries;
    let page: Vec<_> = ranked
        .iter()
        .enumerate()
        .skip(offset.min(COMMUNITY_POSITIONS) as usize)
        .take(limit as usize)
        .collect();
    let ids: Vec<i64> = page.iter().map(|(_, r)| r.entity_id).collect();
    let names: HashMap<i64, charts_db::Named> = charts_db::names(&state.db, kind, &ids)
        .await?
        .into_iter()
        .map(|n| (n.entity_id, n))
        .collect();
    let info = CommunityChartInfo {
        period: range.period,
        from,
        to: range.to,
        following,
        accounts: accounts.len() as i64,
        min_listeners,
        total: ranked.len() as i64,
        offset,
        limit,
    };
    let entries = page
        .into_iter()
        .filter_map(|(i, r)| Some((i as i32 + 1, r, names.get(&r.entity_id)?)));
    let body = match kind {
        CommunityKind::Artists => Json(CommunityArtistChart {
            info,
            entries: entries
                .map(|(position, r, n)| RankedArtist {
                    position,
                    artist_id: r.entity_id,
                    artist_name: n.name.clone(),
                    image_url: n.image.clone(),
                    listeners: whole(r.listeners),
                    plays: whole(r.weight),
                })
                .collect(),
        })
        .into_response(),
        CommunityKind::Tracks => Json(CommunityTrackChart {
            info,
            entries: entries
                .map(|(position, r, n)| RankedTrack {
                    position,
                    track_id: r.entity_id,
                    track_title: n.name.clone(),
                    artist_id: n.artist_id,
                    artist_name: n.artist_name.clone(),
                    album_image: n.image.clone(),
                    listeners: whole(r.listeners),
                    plays: whole(r.weight),
                })
                .collect(),
        })
        .into_response(),
        CommunityKind::Albums => Json(CommunityAlbumChart {
            info,
            entries: entries
                .map(|(position, r, n)| RankedAlbum {
                    position,
                    album_id: r.entity_id,
                    album_title: n.name.clone(),
                    artist_id: n.artist_id,
                    artist_name: n.artist_name.clone(),
                    image_url: n.image.clone(),
                    listeners: whole(r.listeners),
                    plays: whole(r.weight),
                })
                .collect(),
        })
        .into_response(),
    };
    Ok(with_community_caching(body, &etag))
}

/// Keyed by the accounts themselves (any viewer covering the same ones
/// gets the same ranking): a follow, an unfollow, or an account turning
/// private or leaving changes them, so no entry needs invalidating.
fn community_key(
    kind: CommunityKind,
    from: NaiveDate,
    to: NaiveDate,
    min: i64,
    accounts: &[i64],
) -> String {
    let mut sorted = accounts.to_vec();
    sorted.sort_unstable();
    let mut hash = Sha256::new();
    for id in sorted {
        hash.update(id.to_le_bytes());
    }
    let kind = match kind {
        CommunityKind::Artists => "artists",
        CommunityKind::Tracks => "tracks",
        CommunityKind::Albums => "albums",
    };
    format!(
        "community:v1:{kind}:{from}:{to}:{min}:{}",
        &hex::encode(hash.finalize())[..32]
    )
}

/// The ranking stored under `key` within the last five minutes, or a new
/// one. A credit is one account at most, so fewer accounts than the
/// minimum make none. Redis failing only costs the computation.
async fn community_ranking(
    state: &AppState,
    key: &str,
    kind: CommunityKind,
    accounts: &[i64],
    from: NaiveDate,
    to: NaiveDate,
    min_listeners: i64,
) -> ApiResult<Computed> {
    match state.redis.get::<Option<String>, _>(key).await {
        Ok(Some(json)) => match serde_json::from_str(&json) {
            Ok(cached) => return Ok(cached),
            Err(e) => tracing::warn!("unreadable cached community ranking: {e}"),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!("community ranking cache unavailable: {e}"),
    }
    let entries = if (accounts.len() as i64) < min_listeners {
        Vec::new()
    } else {
        charts_db::community(
            &state.db,
            kind,
            accounts,
            from,
            to,
            min_listeners * FULL,
            COMMUNITY_POSITIONS,
        )
        .await?
    };
    let computed = Computed {
        computed_at: Utc::now(),
        entries,
    };
    if let Ok(json) = serde_json::to_string(&computed) {
        let stored = state
            .redis
            .set::<(), _, _>(
                key,
                json,
                Some(Expiration::EX(COMMUNITY_CACHE_SECS)),
                None,
                false,
            )
            .await;
        if let Err(e) = stored {
            tracing::warn!("community ranking cache unavailable: {e}");
        }
    }
    Ok(computed)
}

fn with_community_caching(mut response: Response, etag: &str) -> Response {
    let headers = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(COMMUNITY_CACHE_CONTROL),
    );
    headers.append(header::VARY, HeaderValue::from_static("Authorization"));
    response
}

fn community_headers<T>(r: TransformResponse<'_, T>) -> TransformResponse<'_, T> {
    caching::headers(
        r,
        "The ranking's version, for `If-None-Match`",
        COMMUNITY_CACHE_CONTROL,
    )
}

fn community_doc<'t>(
    op: TransformOperation<'t>,
    summary: &str,
    things: &str,
) -> TransformOperation<'t> {
    let max = charts::community_accounts(7, super::users::MAX_FOLLOWING);
    let year = charts::community_accounts(365, super::users::MAX_FOLLOWING);
    op.summary(summary)
        .description(&format!(
            "The {things} the accounts the signed-in user follows listened to most over a period \
             (`7d` by default, `30d`, `90d` or `12m`: UTC days ending today, today included) or \
             a custom range of UTC days (`from` and `to`, both included, 366 days at most, within \
             the last year: older weights aren't kept, hence no `all`), ranked as the global \
             rankings are: by listeners, then by plays, each account counting once at most, and \
             less when its plays don't look like real listening, plays weighed the same way, and \
             both rounded.\n\n\
             It covers the accounts followed that anyone may see (`following`; private ones and \
             those waiting to be deleted are left out), the most recently active first, as many \
             as the range allows, so a chart's cost doesn't grow with the follows: {max} over a \
             week, {year} over a year (`accounts`). No account is named, and entities fewer than \
             `min_listeners` of those accounts played are left out, so no entry stands for one \
             or two people's listening: following fewer accounts than that gives an empty chart. At most \
             {COMMUNITY_POSITIONS} entries; paged by `limit` (50 by default, 200 at most) and \
             `offset`.\n\n\
             A ranking, once computed, is reused for {} minutes by every request covering the \
             same accounts and days, so pages of one chart agree and positions are that old at \
             most; following or unfollowing, or an account turning private, makes another. \
             Pages carry a weak `ETag` and `Cache-Control: {COMMUNITY_CACHE_CONTROL}`: send the \
             tag back in `If-None-Match` for a 304 while the ranking is the same.",
            COMMUNITY_CACHE_SECS / 60
        ))
        .tag("Charts")
        .with(|op| {
            caching::if_none_match(
                op,
                "The `ETag` of a page fetched before: 304 if the ranking hasn't changed since",
            )
        })
        .response_with::<304, (), _>(|r| {
            community_headers(r).description("Not modified since the `ETag` in `If-None-Match`")
        })
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("Unknown period, `all`, a period with a range, an invalid, too long or too old range, or a negative offset")
        })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
    use serde_json::{Value, json};

    use crate::test_app::{TestApp, with_app};
    use db::queries::classification as cdb;
    use db::queries::tracks as tracks_db;
    use shared::classification::BudgetParams;

    fn days_ago(n: i64) -> NaiveDate {
        (Utc::now() - TimeDelta::days(n)).date_naive()
    }

    fn at(day: NaiveDate, secs: i64) -> DateTime<Utc> {
        day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs)
    }

    /// `plays` plays of the track 4 minutes apart from noon of `day`.
    async fn play(app: &TestApp, user: i64, track: (i64, i64), day: NaiveDate, plays: i64) {
        for n in 0..plays {
            sqlx::query(
                "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
                 VALUES ($1, $2, $3, $4, 'test')",
            )
            .bind(user)
            .bind(track.0)
            .bind(track.1)
            .bind(at(day, 43_200 + n * 240))
            .execute(&app.pool)
            .await
            .unwrap();
        }
    }

    async fn track(
        app: &TestApp,
        artist: &str,
        album: Option<&str>,
        title: &str,
        length: Option<i32>,
    ) -> (i64, i64) {
        let artist = tracks_db::find_or_create_artist(&app.pool, artist)
            .await
            .unwrap();
        let album = match album {
            Some(album) => Some(
                tracks_db::find_or_create_album(&app.pool, artist.id, album)
                    .await
                    .unwrap(),
            ),
            None => None,
        };
        let track = tracks_db::find_or_create_track(&app.pool, artist.id, album, title, length)
            .await
            .unwrap();
        (track.id, artist.id)
    }

    async fn get(app: &TestApp, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
        match bearer {
            Some(bearer) => app.api_as(bearer, Method::GET, path, None).await,
            None => {
                let (status, _, body) = app.get(path).await;
                (status, serde_json::from_str(&body).unwrap_or(Value::Null))
            }
        }
    }

    fn names(chart: &Value, field: &str) -> Vec<(String, i64)> {
        chart["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e[field].as_str().unwrap().to_owned(),
                    e["play_count"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn personal_charts_cover_periods_and_ranges_and_page() {
        with_app(Default::default(), |app| async move {
            let side_a = track(&app, "Band", Some("LP"), "Side A", Some(200_000)).await;
            let side_b = track(&app, "Band", Some("LP"), "Side B", Some(200_000)).await;
            let single = track(&app, "Solo", None, "Single", None).await;
            let user = app.user_id;
            play(&app, user, side_a, days_ago(0), 3).await;
            play(&app, user, side_b, days_ago(3), 2).await;
            play(&app, user, single, days_ago(3), 1).await;
            play(&app, user, side_a, days_ago(20), 4).await;
            play(&app, user, single, days_ago(100), 5).await;
            play(&app, user, side_b, days_ago(400), 6).await;
            // The same listen of Side B reported again 5 s later.
            sqlx::query(
                "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
                 VALUES ($1, $2, $3, $4, 'test')",
            )
            .bind(user)
            .bind(side_b.0)
            .bind(side_b.1)
            .bind(at(days_ago(3), 43_205))
            .execute(&app.pool)
            .await
            .unwrap();
            let rules = cdb::register_ruleset(&app.pool, BudgetParams::default())
                .await
                .unwrap();
            cdb::classify_user_day(&app.pool, &rules, user, days_ago(3), false)
                .await
                .unwrap();
            db::queries::scrobbles::refresh_scrobble_aggregates(
                &app.pool,
                at(days_ago(400), 0),
                Utc::now(),
            )
            .await
            .unwrap();

            let base = format!("/v1/user/{}/charts", app.username);
            let (status, week) = get(&app, &format!("{base}/artists"), None).await;
            assert_eq!(status, StatusCode::OK, "{week}");
            assert_eq!(week["period"], "7d");
            assert_eq!(week["from"], days_ago(6).to_string());
            assert_eq!(week["to"], days_ago(0).to_string());
            assert_eq!(
                names(&week, "artist_name"),
                [("Band".into(), 5), ("Solo".into(), 1)]
            );
            // The untimed single is unverified (no_data); today's plays
            // aren't classified yet, so they're not.
            assert_eq!(week["entries"][1]["unverified_count"], 1);
            assert_eq!(week["entries"][0]["unverified_count"], 0);

            for (period, band, solo) in
                [("30d", 9, 1), ("90d", 9, 1), ("12m", 9, 6), ("all", 15, 6)]
            {
                let (_, chart) = get(&app, &format!("{base}/artists?period={period}"), None).await;
                let mut expected = vec![("Band".to_string(), band), ("Solo".to_string(), solo)];
                expected.sort_by_key(|e| std::cmp::Reverse(e.1));
                assert_eq!(names(&chart, "artist_name"), expected, "{period}");
            }
            let (_, all) = get(&app, &format!("{base}/artists?period=all"), None).await;
            assert_eq!(all["from"], json!(null));

            let (_, tracks) = get(&app, &format!("{base}/tracks?period=all"), None).await;
            assert_eq!(
                names(&tracks, "track_title"),
                [
                    ("Side B".into(), 8),
                    ("Side A".into(), 7),
                    ("Single".into(), 6)
                ]
            );
            let (_, albums) = get(&app, &format!("{base}/albums?period=all"), None).await;
            assert_eq!(names(&albums, "album_title"), [("LP".into(), 15)]);
            assert_eq!(albums["entries"][0]["artist_name"], "Band");

            let day = days_ago(100);
            let (_, range) = get(&app, &format!("{base}/tracks?from={day}&to={day}"), None).await;
            assert_eq!(range["period"], json!(null));
            assert_eq!(names(&range, "track_title"), [("Single".into(), 5)]);

            // Pages.
            let (_, page) = get(
                &app,
                &format!("{base}/tracks?period=all&limit=1&offset=1"),
                None,
            )
            .await;
            assert_eq!(
                (page["total"].as_i64(), page["limit"].as_i64()),
                (Some(3), Some(1))
            );
            assert_eq!(names(&page, "track_title"), [("Side A".into(), 7)]);
            let (_, past) = get(&app, &format!("{base}/tracks?period=all&offset=3"), None).await;
            assert_eq!(
                (past["total"].as_i64(), past["entries"].clone()),
                (Some(3), json!([]))
            );
            let (_, big) = get(&app, &format!("{base}/tracks?limit=100000"), None).await;
            assert_eq!(big["limit"], 200);

            let today = days_ago(0);
            let tomorrow = today + TimeDelta::days(1);
            let year_ago = today - TimeDelta::days(366);
            for bad in [
                "period=week".to_string(),
                format!("period=7d&from={today}&to={today}"),
                format!("from={today}"),
                format!("from={today}&to={}", days_ago(1)),
                format!("from={today}&to={tomorrow}"),
                format!("from={year_ago}&to={today}"),
                format!("from=2026-02-30&to={today}"),
                "offset=-1".to_string(),
            ] {
                let (status, body) = get(&app, &format!("{base}/albums?{bad}"), None).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
            }
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn personal_charts_follow_the_profiles_privacy_and_revalidate() {
        with_app(Default::default(), |app| async move {
            let song = track(&app, "Band", None, "Song", Some(200_000)).await;
            play(&app, app.user_id, song, days_ago(1), 2).await;
            let (_, other, other_session) = app.other_user("other").await;
            let path = format!("/v1/user/{}/charts/artists", app.username);

            let (status, _, body) = app.get(&path).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let request = |etag: &str| {
                Request::get(&path)
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap()
            };
            let (_, headers, _) = app.get(&path).await;
            assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=60");
            let etag = headers[header::ETAG].to_str().unwrap().to_owned();
            let (status, again, body) = app.send(request(&etag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);
            assert_eq!(
                (again[header::ETAG].to_str().unwrap(), body.as_str()),
                (etag.as_str(), "")
            );
            play(&app, app.user_id, song, days_ago(0), 1).await;
            let (status, changed, body) = app.send(request(&etag)).await;
            assert_eq!(status, StatusCode::OK);
            assert_ne!(changed[header::ETAG], etag.as_str());
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["entries"][0]["play_count"], 3);

            app.api(
                Method::PATCH,
                "/v1/user/me",
                Some(json!({ "is_private": true })),
            )
            .await;
            for kind in ["artists", "tracks", "albums"] {
                let path = format!("/v1/user/{}/charts/{kind}", app.username);
                assert_eq!(
                    get(&app, &path, None).await.0,
                    StatusCode::FORBIDDEN,
                    "{kind}"
                );
                assert_eq!(
                    get(&app, &path, Some(&other_session)).await.0,
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    app.api(Method::GET, &path, None).await.0,
                    StatusCode::OK,
                    "{kind}"
                );
                // Even a cached tag gets nothing.
                let (status, _, _) = app.send(request(&etag)).await;
                assert_eq!(status, StatusCode::FORBIDDEN);
            }

            sqlx::query(
                "UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE username = $1",
            )
            .bind(&other)
            .execute(&app.pool)
            .await
            .unwrap();
            for path in [
                format!("/v1/user/{other}/charts/tracks"),
                "/v1/user/nobody/charts/tracks".into(),
            ] {
                assert_eq!(
                    get(&app, &path, None).await.0,
                    StatusCode::NOT_FOUND,
                    "{path}"
                );
            }
        })
        .await;
    }

    /// `n` accounts, 30 days old, each weighing a full listener of the
    /// returned artist's track yesterday.
    async fn listeners(app: &TestApp, n: usize, artist: &str) -> (Vec<i64>, i64) {
        let ruleset = db::queries::rankings::register_ruleset(
            &app.pool,
            &shared::ranking::RankingParams::default(),
        )
        .await
        .unwrap();
        let song = track(app, artist, Some("Record"), "Song", Some(200_000)).await;
        let mut ids = Vec::new();
        for _ in 0..n {
            let (id, _, _) = app.other_user("fan").await;
            sqlx::query("UPDATE users SET created_at = NOW() - INTERVAL '30 days', last_seen_at = NOW() WHERE id = $1")
                .bind(id)
                .execute(&app.pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO ranking_days (user_id, day, ruleset_id, classified_at, plays, weighted, weight,
                     unclassified, suspect, duplicate, no_data, imported, new_account, short_listen,
                     no_listened, unknown_client, track_cap, artist_cap)
                 VALUES ($1, $2, $3, NOW(), 2, 2, 2000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)",
            )
            .bind(id)
            .bind(days_ago(1))
            .bind(ruleset.id)
            .execute(&app.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
                 VALUES ($1, $2, $3, $4, 2, 2000)",
            )
            .bind(id)
            .bind(days_ago(1))
            .bind(song.0)
            .bind(song.1)
            .execute(&app.pool)
            .await
            .unwrap();
            ids.push(id);
        }
        (ids, song.1)
    }

    async fn follow_all(app: &TestApp, ids: &[i64]) {
        sqlx::query(
            "INSERT INTO user_follows (follower_id, followee_id) SELECT $1, unnest($2::bigint[])",
        )
        .bind(app.user_id)
        .bind(ids)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn community_charts_cover_visible_follows_past_the_minimum() {
        with_app(Default::default(), |app| async move {
            let (fans, band) = listeners(&app, 7, "Band").await;
            let (few, _) = listeners(&app, 4, "Niche").await;
            follow_all(&app, &fans).await;
            follow_all(&app, &few).await;
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(fans[0])
                .execute(&app.pool)
                .await
                .unwrap();
            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(fans[1])
                .execute(&app.pool)
                .await
                .unwrap();

            let (status, chart) = app
                .api(Method::GET, "/v1/user/me/community/artists", None)
                .await;
            assert_eq!(status, StatusCode::OK, "{chart}");
            assert_eq!(
                (
                    &chart["following"],
                    &chart["accounts"],
                    &chart["min_listeners"]
                ),
                (&json!(9), &json!(9), &json!(5))
            );
            // Five visible listeners of Band; Niche's four aren't enough.
            assert_eq!(chart["total"], 1);
            assert_eq!(chart["entries"][0]["artist_id"], band);
            assert_eq!(chart["entries"][0]["position"], 1);
            assert_eq!(
                (
                    &chart["entries"][0]["listeners"],
                    &chart["entries"][0]["plays"]
                ),
                (&json!(5), &json!(10))
            );
            let (_, tracks) = app
                .api(Method::GET, "/v1/user/me/community/tracks?period=30d", None)
                .await;
            assert_eq!(tracks["entries"][0]["track_title"], "Song");
            let (_, albums) = app
                .api(Method::GET, "/v1/user/me/community/albums?period=12m", None)
                .await;
            assert_eq!(albums["entries"][0]["album_title"], "Record");
            assert_eq!(albums["entries"][0]["artist_name"], "Band");

            // The same ranking again, from the cache: a 304 on its tag.
            let path = "/v1/user/me/community/artists";
            let request = |etag: Option<&str>| {
                let mut req = Request::get(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", app.session));
                if let Some(etag) = etag {
                    req = req.header(header::IF_NONE_MATCH, etag);
                }
                req.body(Body::empty()).unwrap()
            };
            let (_, headers, _) = app.send(request(None)).await;
            assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=60");
            assert!(
                headers
                    .get_all(header::VARY)
                    .iter()
                    .any(|v| v == "Authorization")
            );
            let etag = headers[header::ETAG].to_str().unwrap().to_owned();
            let (status, _, _) = app.send(request(Some(&etag))).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);

            // One more turns private: under the minimum.
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(fans[2])
                .execute(&app.pool)
                .await
                .unwrap();
            let (status, _, body) = app.send(request(Some(&etag))).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "another set of accounts, another ranking"
            );
            let chart: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                (&chart["total"], &chart["entries"]),
                (&json!(0), &json!([]))
            );

            // Someone following none of them has an empty chart.
            let (_, _, stranger) = app.other_user("stranger").await;
            let (status, chart) = app
                .api_as(&stranger, Method::GET, "/v1/user/me/community/tracks", None)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                (&chart["following"], &chart["entries"]),
                (&json!(0), &json!([]))
            );

            let today = days_ago(0);
            let old = today - TimeDelta::days(400);
            for bad in [
                "period=all".to_string(),
                format!("from={old}&to={}", old + TimeDelta::days(10)),
                "period=7d&from=2026-01-01&to=2026-01-02".into(),
                "offset=-1".into(),
            ] {
                let (status, body) = app
                    .api(
                        Method::GET,
                        &format!("/v1/user/me/community/albums?{bad}"),
                        None,
                    )
                    .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
            }
            let (status, _) = get(&app, "/v1/user/me/community/artists", None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        })
        .await;
    }
}
