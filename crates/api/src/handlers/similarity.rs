//! Similar artists, taste neighbours and recommendations: what the worker
//! stores once a day (`db::queries::similarity`), served without computing
//! anything per request but the filters for what changed since (blocks,
//! privacy, the setting, skips and plays).

use aide::transform::{TransformOperation, TransformResponse};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{IdPath, UsernamePath, caching};
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::similarity::{self as similarity_db, Version};
use db::queries::{tracks as tracks_db, users as users_db};
use shared::models::{
    ArtistRecommendations, ArtistRef, RecommendedArtist, SimilarArtist, SimilarArtists,
    SimilarUser, SimilarUsers, SimilaritySettings, SimilarityStatus, UserSummary,
};
use shared::similarity::{FULL, SKIP_DAYS, SimilarityMode, common_listeners};

pub const ARTIST_LIMIT: i64 = 20;
pub const MAX_ARTIST_LIMIT: i64 = 50;
pub const USER_LIMIT: i64 = 10;
pub const MAX_USER_LIMIT: i64 = 50;
pub const RECOMMENDATION_LIMIT: i64 = 20;
pub const MAX_RECOMMENDATION_LIMIT: i64 = 100;

/// Recomputed once a day; a revalidation costs a primary-key lookup.
const PUBLIC_CACHE_CONTROL: &str = "public, max-age=3600, stale-while-revalidate=3600";
/// Before the worker's first run.
const MISSING_CACHE_CONTROL: &str = "public, max-age=60";
/// Changes with the user's plays, skips and blocks: revalidated each time.
const PERSONAL_CACHE_CONTROL: &str = "private, no-cache";
const NEIGHBOURS_CACHE_CONTROL: &str = "private, max-age=60";

fn score(thousandths: i64) -> f64 {
    thousandths as f64 / FULL as f64
}

fn computed(version: Option<&Version>, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, bool) {
    (
        version.map(|v| v.finished_at),
        version.is_some_and(|v| v.stale(now)),
    )
}

fn with_cache_control(mut response: Response, value: &'static str) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(etag) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    response
}

fn page(offset: Option<i64>, limit: Option<i64>, default: i64, max: i64) -> ApiResult<(i64, i64)> {
    let offset = offset.unwrap_or(0);
    if offset < 0 {
        return Err(AppError::BadRequest("offset can't be negative".into()));
    }
    Ok((offset, super::page_limit(limit, default, max)))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarArtistsQuery {
    /// Entries per page: 20 by default, 50 at most.
    pub limit: Option<i64>,
    /// Positions to skip, 0 by default.
    pub offset: Option<i64>,
}

/// GET /v1/artist/{id}/similar
pub async fn similar_artists(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SimilarArtistsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (offset, limit) = page(q.offset, q.limit, ARTIST_LIMIT, MAX_ARTIST_LIMIT)?;
    let now = Utc::now();
    if let Some(tags) = headers.get(header::IF_NONE_MATCH)
        && let Some(version) = similarity_db::version(&state.db).await?
    {
        let etag = public_etag(&version, now);
        if caching::matches(tags, &etag) {
            let response = with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag);
            return Ok(with_cache_control(response, PUBLIC_CACHE_CONTROL));
        }
    }
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let page = similarity_db::artist_page(&state.db, id, offset, limit).await?;
    let (computed_at, stale) = computed(page.as_ref().map(|p| &p.version), now);
    let body = SimilarArtists {
        artist_id: id,
        computed_at,
        stale,
        total: page.as_ref().map_or(0, |p| p.total),
        offset,
        limit,
        entries: page
            .as_ref()
            .map(|p| {
                p.entries
                    .iter()
                    .map(|e| SimilarArtist {
                        position: e.position,
                        artist_id: e.artist_id,
                        artist_name: e.name.clone(),
                        image_url: e.image_url.clone(),
                        score: score(e.score),
                        common_listeners: common_listeners(e.listeners),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let response = Json(body).into_response();
    Ok(match page {
        Some(p) => with_cache_control(
            with_etag(response, &public_etag(&p.version, now)),
            PUBLIC_CACHE_CONTROL,
        ),
        None => with_cache_control(response, MISSING_CACHE_CONTROL),
    })
}

/// Weak, since compression changes the bytes: the run and whether it is
/// stale, which is all that changes the body of a URL.
fn public_etag(v: &Version, now: DateTime<Utc>) -> String {
    format!(
        "W/\"sa1-{}-{}-{}\"",
        v.run_id,
        v.finished_at.timestamp_micros(),
        if v.stale(now) { "s" } else { "f" }
    )
}

pub fn _similar_artists_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Similar artists")
        .description(
            "Artists whose listeners also play this one, the most similar first: their \
             audiences overlap more than chance would have them, against their sizes. Each \
             account counts once at most, whatever it plays, and less when its plays don't look \
             like real listening; an artist is only listed when at least 10 accounts listen to \
             both, and a tenth of this artist's listeners also play it, so a few accounts can't \
             attach one artist to another. `common_listeners` is a lower bound (10, 20, 50, \
             100, …), never an exact count, and nothing names an account.\n\n\
             The server recomputes them once a day: `computed_at` says when, and when a \
             recomputation is overdue the last ones are still served, with `stale: true`. An \
             artist with too few listeners has none (`total: 0`); before the first computation \
             `computed_at` is `null`.\n\n\
             Pages carry an `ETag` and may be cached for an hour; send the tag back in \
             `If-None-Match` for a 304 while they haven't changed.",
        )
        .tag("Recommendations")
        .with(|op| {
            caching::if_none_match(
                op,
                "The `ETag` of a page fetched before: 304 if they haven't changed since",
            )
        })
        .response_with::<200, Json<SimilarArtists>, _>(|r| {
            public_headers(r).description("A page of the similar artists")
        })
        .response_with::<304, (), _>(|r| {
            public_headers(r).description("Not modified since the `ETag` in `If-None-Match`")
        })
        .response_with::<400, ErrorJson, _>(|r| r.description("Negative offset"))
        .response_with::<404, ErrorJson, _>(|r| r.description("No such artist"))
}

fn public_headers<T>(r: TransformResponse<'_, T>) -> TransformResponse<'_, T> {
    caching::headers(
        r,
        "The version of the similar artists, for `If-None-Match`",
        PUBLIC_CACHE_CONTROL,
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarUsersQuery {
    /// Entries: 10 by default, 50 at most.
    pub limit: Option<i64>,
}

/// GET /v1/user/{username}/similar
pub async fn similar_users(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<SimilarUsersQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Response> {
    let limit = super::page_limit(q.limit, USER_LIMIT, MAX_USER_LIMIT);
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;
    let setting = similarity_db::setting(&state.db, user.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let own = viewer_id == Some(user.id);
    // Someone else's neighbours are shown only if they chose to be shown
    // themselves; otherwise there is no such list, as for no such user.
    if !own && setting.mode != SimilarityMode::Listed {
        return Err(AppError::NotFound);
    }
    let now = Utc::now();
    let version = similarity_db::version(&state.db).await?;
    let (computed_at, stale) = computed(version.as_ref(), now);
    let (status, entries) = if setting.mode == SimilarityMode::Off {
        (SimilarityStatus::Off, Vec::new())
    } else if version.is_none() {
        (SimilarityStatus::Pending, Vec::new())
    } else {
        let entries: Vec<SimilarUser> = similarity_db::taste_neighbours(
            &state.db,
            user.id,
            viewer_id.filter(|v| *v != user.id),
        )
        .await?
        .into_iter()
        .take(limit as usize)
        .map(|n| SimilarUser {
            user: UserSummary {
                id: n.user_id,
                username: n.username,
                display_name: n.display_name,
                image_url: n.image_url,
            },
            score: score(n.score),
            shared_artists: n.shared_artists,
            top_shared: n
                .top_shared
                .into_iter()
                .map(|(artist_id, artist_name)| ArtistRef {
                    artist_id,
                    artist_name,
                })
                .collect(),
        })
        .collect();
        let status = if entries.is_empty() {
            SimilarityStatus::None
        } else {
            SimilarityStatus::Ready
        };
        (status, entries)
    };
    let body = SimilarUsers {
        status,
        computed_at,
        stale,
        entries,
    };
    let mut response = with_cache_control(Json(body).into_response(), NEIGHBOURS_CACHE_CONTROL);
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Authorization"));
    Ok(response)
}

pub fn _similar_users_doc(op: TransformOperation) -> TransformOperation {
    op.summary("A user's taste neighbours")
        .description(
            "Accounts whose listening is closest to this user's, the closest first, with the \
             artists both listen to (`shared_artists`) and the rarest three of them. Only \
             accounts that chose to be shown appear (`PUT /v1/user/me/similarity` with \
             `listed`), while their profile is public and they aren't waiting to be deleted, \
             and never one blocking or blocked by the user or the signed-in viewer.\n\n\
             The user's own list is theirs to see whatever their setting (`status: off` and no \
             entries if they turned similarity off); someone else's is shown only when that \
             user is listed, and is otherwise not found (404), as a user who doesn't exist. A \
             private profile's list is its owner's alone (403), and an account blocking the \
             viewer or waiting to be deleted is not found, as for its profile.\n\n\
             Recomputed once a day (`computed_at`, `stale` as for similar artists); `status` \
             says whether there is nothing yet (`none`: too little listening, or no listed \
             account close enough) or nothing computed at all (`pending`).",
        )
        .tag("Recommendations")
        .response_with::<200, Json<SimilarUsers>, _>(|r| {
            caching::cache_control(r, NEIGHBOURS_CACHE_CONTROL).description("The neighbours")
        })
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description(
                "User not found, waiting to be deleted, blocking the viewer, or not sharing \
                 their taste neighbours",
            )
        })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecommendationsQuery {
    /// Entries per page: 20 by default, 100 at most.
    pub limit: Option<i64>,
    /// Entries to skip, 0 by default.
    pub offset: Option<i64>,
}

/// GET /v1/user/me/recommendations/artists
pub async fn recommended_artists(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<RecommendationsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (offset, limit) = page(
        q.offset,
        q.limit,
        RECOMMENDATION_LIMIT,
        MAX_RECOMMENDATION_LIMIT,
    )?;
    let db = &state.db;
    let now = Utc::now();
    let setting = similarity_db::setting(db, auth_user.id)
        .await?
        .ok_or(AppError::NotFound)?;
    // Read before the list: a run landing between them can only make the
    // tag older than the body, never the other way round.
    let version = similarity_db::version(db).await?;
    let (last_play, skips, last_skip) =
        similarity_db::recommendation_state(db, auth_user.id).await?;
    let etag = format!(
        "W/\"ra1-{}-{}-{}-{skips}-{}-{}\"",
        setting.mode.as_str(),
        version.map_or(0, |v| v.run_id),
        last_play.map_or(0, |t| t.timestamp_micros()),
        last_skip.map_or(0, |t| t.timestamp_micros()),
        if version.is_some_and(|v| v.stale(now)) {
            "s"
        } else {
            "f"
        }
    );
    if let Some(tags) = headers.get(header::IF_NONE_MATCH)
        && caching::matches(tags, &etag)
    {
        let response = with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag);
        return Ok(with_cache_control(response, PERSONAL_CACHE_CONTROL));
    }

    let (computed_at, stale) = computed(version.as_ref(), now);
    let mut body = ArtistRecommendations {
        status: SimilarityStatus::Pending,
        computed_at,
        stale,
        total: 0,
        offset,
        limit,
        entries: Vec::new(),
    };
    if setting.mode == SimilarityMode::Off {
        body.status = SimilarityStatus::Off;
    } else if let Some(version) = version {
        let all = similarity_db::recommendations(db, auth_user.id, version.started_at).await?;
        body.status =
            if all.is_empty() && !similarity_db::has_recommendations(db, auth_user.id).await? {
                SimilarityStatus::None
            } else {
                SimilarityStatus::Ready
            };
        let best = all.first().map_or(1, |r| r.score.max(1));
        body.total = all.len() as i64;
        body.entries = all
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(i, r)| RecommendedArtist {
                position: i as i32 + 1,
                artist_id: r.artist_id,
                artist_name: r.name,
                image_url: r.image_url,
                score: (r.score as f64 / best as f64 * 1000.0).round() / 1000.0,
                because: ArtistRef {
                    artist_id: r.because_id,
                    artist_name: r.because_name,
                },
                because_count: r.because_count,
            })
            .collect();
    }
    let response = with_etag(Json(body).into_response(), &etag);
    Ok(with_cache_control(response, PERSONAL_CACHE_CONTROL))
}

pub fn _recommended_artists_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Artists recommended to me")
        .description(&format!(
            "Artists the signed-in user hasn't listened to, ranked by how much the listeners of \
             the artists they listen to also play them. Each says why, with one of the user's \
             own artists: \"listeners of `because` also play this\", and how many of the \
             user's artists led to it (`because_count`); nothing names another account. The \
             user's whole history counts, imports included, recent listening more (a half-life \
             of 180 days by default); what other accounts play counts as for similar artists.\n\n\
             Recomputed once a day; artists the user played or skipped since are left out at \
             once. `status`: `ready`, `none` (too little listening yet, or nothing similar \
             enough: at least 5 artists are needed by default), `off` (the user turned \
             similarity off) or `pending` (nothing computed yet). Up to 100 recommendations, \
             paged by `limit` (20 by default, 100 at most) and `offset`; `score` is relative \
             to the first.\n\n\
             Responses carry a weak `ETag` and `Cache-Control: {PERSONAL_CACHE_CONTROL}`: send \
             the tag back in `If-None-Match` for a 304 while nothing changed (a new run, a \
             play, a skip)."
        ))
        .tag("Recommendations")
        .with(|op| {
            caching::if_none_match(
                op,
                "The `ETag` of a page fetched before: 304 if nothing changed since",
            )
        })
        .response_with::<200, Json<ArtistRecommendations>, _>(|r| {
            caching::headers(
                r,
                "The version of the recommendations, for `If-None-Match`",
                PERSONAL_CACHE_CONTROL,
            )
            .description("A page of the recommendations")
        })
        .response_with::<304, (), _>(|r| {
            caching::headers(
                r,
                "The version of the recommendations, for `If-None-Match`",
                PERSONAL_CACHE_CONTROL,
            )
            .description("Not modified since the `ETag` in `If-None-Match`")
        })
        .response_with::<400, ErrorJson, _>(|r| r.description("Negative offset"))
}

/// POST /v1/user/me/recommendations/artists/{id}/skip
pub async fn skip_recommendation(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<StatusCode> {
    if !similarity_db::skip(&state.db, auth_user.id, id, SKIP_DAYS).await? {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn _skip_recommendation_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Skip a recommended artist")
        .description(&format!(
            "Hides the artist from the signed-in user's recommendations for {SKIP_DAYS} days, at \
             once; skipping it again starts the {SKIP_DAYS} days over. The newest {} skips are \
             kept. Any artist can be skipped, recommended or not.",
            similarity_db::MAX_SKIPS
        ))
        .tag("Recommendations")
        .response_with::<204, (), _>(|r| r.description("Skipped"))
        .response_with::<404, ErrorJson, _>(|r| r.description("No such artist"))
}

/// DELETE /v1/user/me/recommendations/artists/{id}/skip
pub async fn unskip_recommendation(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<StatusCode> {
    similarity_db::unskip(&state.db, auth_user.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _unskip_recommendation_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Take back a skip")
        .description(
            "The artist may be recommended again, from the next request if it still is. 204 \
             also when it wasn't skipped.",
        )
        .tag("Recommendations")
        .response_with::<204, (), _>(|r| r.description("No longer skipped"))
}

fn settings_body(setting: similarity_db::Setting) -> SimilaritySettings {
    SimilaritySettings {
        mode: setting.mode,
        shown: setting.mode == SimilarityMode::Listed && !setting.is_private,
        changed_at: setting.changed_at,
    }
}

/// GET /v1/user/me/similarity
pub async fn get_settings(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<Json<SimilaritySettings>> {
    let setting = similarity_db::setting(&state.db, auth_user.id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(settings_body(setting)))
}

const MODES_DESCRIPTION: &str = "`listed`: the account's listening counts, anonymously, \
     towards similar artists and others' recommendations, and it may be shown as a taste \
     neighbour of others while its profile is public. `anonymous` (the default): it counts, \
     anonymously, and is never shown as anyone's taste neighbour. `off`: nothing is computed \
     from or for the account: no recommendations and no taste neighbours, and its listening \
     leaves similar artists at the next daily computation.";

pub fn _get_settings_doc(op: TransformOperation) -> TransformOperation {
    op.summary("My similarity setting")
        .description(&format!(
            "How the signed-in user's listening is used for similarity, and whether others see \
             them as a taste neighbour now (`shown`). {MODES_DESCRIPTION}"
        ))
        .tag("Recommendations")
        .response::<200, Json<SimilaritySettings>>()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilaritySettingsRequest {
    /// `listed`, `anonymous` or `off`.
    #[schemars(with = "SimilarityMode")]
    pub mode: String,
}

/// PUT /v1/user/me/similarity
pub async fn put_settings(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<SimilaritySettingsRequest>,
) -> ApiResult<Json<SimilaritySettings>> {
    let mode = SimilarityMode::parse(body.mode.trim()).ok_or_else(|| {
        AppError::BadRequest("mode must be `listed`, `anonymous` or `off`".into())
    })?;
    let setting = similarity_db::set_setting(&state.db, auth_user.id, mode).await?;
    Ok(Json(settings_body(setting)))
}

pub fn _put_settings_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Change my similarity setting")
        .description(&format!(
            "{MODES_DESCRIPTION}\n\n\
             Takes effect at once for what names the account: leaving `listed` removes it from \
             every taste neighbour list, `off` deletes the account's recommendations and taste \
             neighbours. Its anonymous part in similar artists changes at the next daily \
             computation. Takes a session: an API token can't change it."
        ))
        .tag("Recommendations")
        .response::<200, Json<SimilaritySettings>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Unknown mode"))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use crate::test_app::{TestApp, with_app};

    async fn exec(pool: &PgPool, sql: &str) {
        sqlx::query(sql).execute(pool).await.unwrap();
    }

    /// A stored run: Band 1's neighbours Band 2 (37 accounts) and Band 3,
    /// and the test user recommended Band 3 because of Band 1, then Band 4.
    async fn stored(app: &TestApp) -> (i64, i64) {
        exec(
            &app.pool,
            "INSERT INTO artists (name, name_normalized)
             SELECT 'Band ' || n, 'band ' || n FROM generate_series(1, 4) n",
        )
        .await;
        exec(
            &app.pool,
            "INSERT INTO similarity_runs
                 (fingerprint, window_from, window_to, accounts, candidates, artists, neighbours,
                  recommended_users, neighboured_users, refresh_secs, started_at, took_ms)
             VALUES ('f', CURRENT_DATE - 179, CURRENT_DATE, 50, 10, 1, 2, 1, 0, 86400,
                     NOW() - INTERVAL '1 hour', 1)",
        )
        .await;
        let band = |n: i64| format!("(SELECT id FROM artists WHERE name = 'Band {n}')");
        exec(
            &app.pool,
            &format!(
                "INSERT INTO artist_neighbours (artist_id, position, neighbour_id, score, listeners)
                 VALUES ({b1}, 1, {b2}, 412, 37), ({b1}, 2, {b3}, 105, 12)",
                b1 = band(1),
                b2 = band(2),
                b3 = band(3)
            ),
        )
        .await;
        exec(
            &app.pool,
            &format!(
                "INSERT INTO user_recommendations
                     (user_id, position, artist_id, score, because_artist_id, because_count)
                 VALUES ({u}, 1, {b3}, 800, {b1}, 2), ({u}, 2, {b4}, 200, {b1}, 1)",
                u = app.user_id,
                b1 = band(1),
                b3 = band(3),
                b4 = band(4)
            ),
        )
        .await;
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM artists WHERE name IN ('Band 1', 'Band 3') ORDER BY name",
        )
        .fetch_all(&app.pool)
        .await
        .unwrap();
        (ids[0], ids[1])
    }

    async fn get(
        app: &TestApp,
        path: &str,
        etag: Option<&str>,
    ) -> (StatusCode, Value, String, String) {
        let mut req = Request::get(path);
        if let Some(etag) = etag {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        let (status, headers, body) = app.send(req.body(Body::empty()).unwrap()).await;
        let header = |name| {
            headers
                .get(name)
                .map_or(String::new(), |v: &header::HeaderValue| {
                    v.to_str().unwrap().to_string()
                })
        };
        (
            status,
            serde_json::from_str(&body).unwrap_or(Value::Null),
            header(header::ETAG),
            header(header::CACHE_CONTROL),
        )
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn similar_artists_are_public_coarse_and_revalidate() {
        with_app(Default::default(), |app| async move {
            exec(
                &app.pool,
                "INSERT INTO artists (name, name_normalized) VALUES ('Alone', 'alone')",
            )
            .await;
            let alone: i64 = sqlx::query_scalar("SELECT id FROM artists")
                .fetch_one(&app.pool)
                .await
                .unwrap();
            // Before the first run: nothing yet, briefly cacheable.
            let (status, body, _, cache) =
                get(&app, &format!("/v1/artist/{alone}/similar"), None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["computed_at"], Value::Null);
            assert_eq!(body["entries"], json!([]));
            assert_eq!(cache, "public, max-age=60");

            let (band1, _) = stored(&app).await;
            let path = format!("/v1/artist/{band1}/similar");
            let (status, body, etag, cache) = get(&app, &path, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["total"], 2);
            assert_eq!(body["stale"], false);
            assert_eq!(body["entries"][0]["artist_name"], "Band 2");
            assert_eq!(body["entries"][0]["score"], 0.412);
            assert_eq!(body["entries"][0]["common_listeners"], 20);
            assert_eq!(body["entries"][1]["common_listeners"], 10);
            assert!(cache.starts_with("public, max-age=3600"));
            let (status, _, again, _) = get(&app, &path, Some(&etag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);
            assert_eq!(again, etag);
            let (_, page, _, _) = get(&app, &format!("{path}?offset=1&limit=1"), None).await;
            assert_eq!(page["entries"][0]["position"], 2);
            assert_eq!(page["total"], 2);

            let (status, body, _, _) =
                get(&app, &format!("/v1/artist/{alone}/similar"), None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["total"], 0);
            assert!(body["computed_at"].is_string());
            assert_eq!(
                get(&app, "/v1/artist/0/similar", None).await.0,
                StatusCode::NOT_FOUND
            );
            let (status, _, _, _) = get(&app, &format!("{path}?offset=-1"), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn recommendations_leave_out_skips_and_plays_and_revalidate() {
        with_app(Default::default(), |app| async move {
            let (status, body) = app
                .api(Method::GET, "/v1/user/me/recommendations/artists", None)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["status"], "pending");
            let (band1, band3) = stored(&app).await;
            let path = "/v1/user/me/recommendations/artists";
            let (status, body) = app.api(Method::GET, path, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], "ready");
            assert_eq!(body["total"], 2);
            let first = &body["entries"][0];
            assert_eq!(first["artist_name"], "Band 3");
            assert_eq!(first["score"], 1.0);
            assert_eq!(first["because"]["artist_id"], band1);
            assert_eq!(first["because"]["artist_name"], "Band 1");
            assert_eq!(first["because_count"], 2);
            assert_eq!(body["entries"][1]["score"], 0.25);

            let send = |etag: Option<String>| {
                let mut req = Request::get(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", app.session));
                if let Some(etag) = etag {
                    req = req.header(header::IF_NONE_MATCH, etag);
                }
                app.send(req.body(Body::empty()).unwrap())
            };
            let (_, headers, _) = send(None).await;
            let etag = headers[header::ETAG].to_str().unwrap().to_string();
            assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
            assert_eq!(send(Some(etag.clone())).await.0, StatusCode::NOT_MODIFIED);

            // Skipped: gone at once, and the tag changes.
            let skip = format!("/v1/user/me/recommendations/artists/{band3}/skip");
            assert_eq!(app.api(Method::POST, &skip, None).await.0, StatusCode::NO_CONTENT);
            assert_eq!(send(Some(etag)).await.0, StatusCode::OK);
            let (_, body) = app.api(Method::GET, path, None).await;
            assert_eq!(body["total"], 1);
            assert_eq!(body["entries"][0]["artist_name"], "Band 4");
            assert_eq!(body["entries"][0]["score"], 1.0);
            assert_eq!(app.api(Method::DELETE, &skip, None).await.0, StatusCode::NO_CONTENT);
            assert_eq!(app.api(Method::GET, path, None).await.1["total"], 2);
            let missing = "/v1/user/me/recommendations/artists/0/skip";
            assert_eq!(app.api(Method::POST, missing, None).await.0, StatusCode::NOT_FOUND);

            // Played since: gone.
            let (status, _) = app
                .api(
                    Method::POST,
                    "/v1/scrobble",
                    Some(json!({ "track": "Song", "artist": "Band 4", "played_at": chrono::Utc::now() })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED);
            let (_, body) = app.api(Method::GET, path, None).await;
            assert_eq!(body["total"], 1);
            assert_eq!(body["entries"][0]["artist_name"], "Band 3");

            // Off: nothing is kept, nothing served.
            let (status, setting) = app
                .api(Method::PUT, "/v1/user/me/similarity", Some(json!({ "mode": "off" })))
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(setting, json!({ "mode": "off", "shown": false, "changed_at": setting["changed_at"] }));
            let (_, body) = app.api(Method::GET, path, None).await;
            assert_eq!(body["status"], "off");
            assert_eq!(body["entries"], json!([]));
            let left: i64 = sqlx::query_scalar("SELECT count(*) FROM user_recommendations")
                .fetch_one(&app.pool)
                .await
                .unwrap();
            assert_eq!(left, 0);
            let (status, _) = app
                .api(Method::PUT, "/v1/user/me/similarity", Some(json!({ "mode": "everyone" })))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn taste_neighbours_follow_the_setting_visibility_and_blocks() {
        with_app(Default::default(), |app| async move {
            stored(&app).await;
            let (owner, owner_name, owner_session) = app.other_user("ownerqz").await;
            let (listed, listed_name, listed_session) = app.other_user("listedqz").await;
            let (shy, _, _) = app.other_user("shyqz").await;
            let (viewer, _, viewer_session) = app.other_user("viewerqz").await;
            exec(
                &app.pool,
                &format!("UPDATE users SET similarity = 'listed' WHERE id IN ({listed}, {viewer})"),
            )
            .await;
            exec(
                &app.pool,
                &format!(
                    "INSERT INTO user_neighbours
                     (user_id, position, neighbour_user_id, score, shared_artists, top_shared)
                 SELECT {owner}, 1, {listed}, 731, 12,
                        ARRAY(SELECT id FROM artists WHERE name LIKE 'Band %' ORDER BY name LIMIT 2)
                 UNION ALL SELECT {owner}, 2, {shy}, 500, 5, '{{}}'
                 UNION ALL SELECT {owner}, 3, {viewer}, 400, 4, '{{}}'"
                ),
            )
            .await;
            let path = format!("/v1/user/{owner_name}/similar");

            // The owner sees theirs; others don't while the owner isn't listed.
            let (status, body) = app.api_as(&owner_session, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["status"], "ready");
            let names: Vec<&str> = body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["user"]["username"].as_str().unwrap())
                .collect();
            assert_eq!(names.len(), 2, "{body}");
            assert_eq!(names[0], listed_name);
            assert_eq!(body["entries"][0]["score"], 0.731);
            assert_eq!(body["entries"][0]["shared_artists"], 12);
            assert_eq!(body["entries"][0]["top_shared"][0]["artist_name"], "Band 1");
            let (status, missing) = app.api(Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (_, nobody) = app
                .api(Method::GET, "/v1/user/nobodyqz/similar", None)
                .await;
            assert_eq!(missing, nobody);
            assert_eq!(get(&app, &path, None).await.0, StatusCode::NOT_FOUND);

            // Listed: anyone sees it, without accounts blocking the viewer.
            app.api_as(
                &owner_session,
                Method::PUT,
                "/v1/user/me/similarity",
                Some(json!({ "mode": "listed" })),
            )
            .await;
            let (status, body) = app.api(Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["entries"].as_array().unwrap().len(), 2);
            app.api_as(
                &listed_session,
                Method::POST,
                &format!("/v1/user/{}/block", app.username),
                None,
            )
            .await;
            let (_, body) = app.api(Method::GET, &path, None).await;
            assert_eq!(body["entries"].as_array().unwrap().len(), 1);
            let (_, body) = app.api_as(&viewer_session, Method::GET, &path, None).await;
            assert_eq!(body["entries"].as_array().unwrap().len(), 2);

            // A listed neighbour turning private leaves every list.
            exec(
                &app.pool,
                &format!("UPDATE users SET is_private = TRUE WHERE id = {listed}"),
            )
            .await;
            let (_, body) = app.api_as(&owner_session, Method::GET, &path, None).await;
            assert_eq!(body["entries"].as_array().unwrap().len(), 1);
            // The owner going private: their list is theirs alone again.
            exec(
                &app.pool,
                &format!("UPDATE users SET is_private = TRUE WHERE id = {owner}"),
            )
            .await;
            assert_eq!(
                app.api(Method::GET, &path, None).await.0,
                StatusCode::FORBIDDEN
            );

            let (status, settings) = app
                .api_as(&owner_session, Method::GET, "/v1/user/me/similarity", None)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(settings["mode"], "listed");
            assert_eq!(settings["shown"], false, "private, so not shown");
        })
        .await;
    }
}
