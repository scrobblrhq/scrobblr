//! Charts: a user's own top artists, tracks and albums over a period or a
//! range of UTC days (`db::queries::charts`).

use aide::transform::{TransformOperation, TransformResponse};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, Utc};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{UsernamePath, caching};
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::{charts as charts_db, users as users_db};
use shared::charts::{self, ChartPeriod, ChartRange, RangeQuery, RangeRules};
use shared::models::{AlbumChart, ArtistChart, ChartInfo, ScrobbleBreakdown, TrackChart};

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

/// Per viewer: a private profile's chart is its owner's alone, and any
/// profile can turn private.
const PERSONAL_CACHE_CONTROL: &str = "private, max-age=60";

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
            Json(ArtistChart {
                info: info(page.total),
                entries: page.entries,
            })
            .into_response()
        }
        Kind::Tracks => {
            let page = charts_db::user_tracks(db, user.id, &range, limit, offset).await?;
            Json(TrackChart {
                info: info(page.total),
                entries: page.entries,
            })
            .into_response()
        }
        Kind::Albums => {
            let page = charts_db::user_albums(db, user.id, &range, limit, offset).await?;
            Json(AlbumChart {
                info: info(page.total),
                entries: page.entries,
            })
            .into_response()
        }
    };
    Ok(with_personal_caching(body, &etag))
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
             `If-None-Match` for a 304 while the chart hasn't changed. Private profiles return \
             403 to everyone but their owner."
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
}
