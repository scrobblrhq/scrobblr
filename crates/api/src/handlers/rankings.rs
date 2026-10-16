//! Global rankings: the pages the worker stores (`db::queries::rankings`),
//! served as they are, without computing anything per request.

use aide::transform::{TransformOperation, TransformResponse};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;

use super::caching;
use crate::{
    errors::{ApiResult, AppError, ErrorBody, ErrorJson},
    state::AppState,
};
use db::queries::rankings::{self as rankings_db, Kind, ServedEntry, SnapshotVersion};
use shared::models::{ArtistRanking, RankedArtist, RankedTrack, RankingInfo, TrackRanking};
use shared::ranking::{self, FULL, Period, whole};

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

/// Rankings change when the worker recomputes them, hourly at most often;
/// a revalidation costs a primary-key lookup.
const CACHE_CONTROL: &str = "public, max-age=300, stale-while-revalidate=300";
/// Before the worker's first snapshot.
const CACHE_CONTROL_MISSING: &str = "public, max-age=60";

/// `RANKING_MIN_LISTENERS` (`shared::ranking::is_served`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankingConfig {
    pub min_listeners: u32,
    /// How old an account must be to edit bios (`RANKING_MIN_ACCOUNT_DAYS`).
    pub min_account_days: i64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            min_listeners: ranking::DEFAULT_MIN_LISTENERS,
            min_account_days: ranking::DEFAULT_MIN_ACCOUNT_DAYS,
        }
    }
}

impl RankingConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name| std::env::var(name).ok();
        Ok(Self {
            min_listeners: ranking::parse_min_listeners(var("RANKING_MIN_LISTENERS").as_deref())
                .map_err(anyhow::Error::msg)?,
            min_account_days: ranking::parse_min_account_days(
                var("RANKING_MIN_ACCOUNT_DAYS").as_deref(),
            )
            .map_err(anyhow::Error::msg)?,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RankingQuery {
    /// `week` (the default), `month` or `year`.
    #[schemars(with = "Option<Period>")]
    pub period: Option<String>,
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// Positions to skip, 0 by default: `offset=50` starts at position 51.
    pub offset: Option<i64>,
}

/// GET /v1/rankings/artists
pub async fn artist_ranking(
    State(state): State<AppState>,
    Query(q): Query<RankingQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    serve(&state, Kind::Artist, q, &headers, |info, entries| {
        Json(ArtistRanking {
            info,
            entries: entries
                .into_iter()
                .map(|e| RankedArtist {
                    position: e.position,
                    artist_id: e.entity_id,
                    artist_name: e.name,
                    image_url: e.image,
                    listeners: whole(e.listeners),
                    plays: whole(e.weight),
                })
                .collect(),
        })
        .into_response()
    })
    .await
}

pub fn _artist_ranking_doc(op: TransformOperation) -> TransformOperation {
    document(op, "Global artist ranking", "artists").response_with::<200, Json<ArtistRanking>, _>(
        |r| caching_headers(r).description("A page of the ranking"),
    )
}

/// GET /v1/rankings/tracks
pub async fn track_ranking(
    State(state): State<AppState>,
    Query(q): Query<RankingQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    serve(&state, Kind::Track, q, &headers, |info, entries| {
        Json(TrackRanking {
            info,
            entries: entries
                .into_iter()
                .map(|e| RankedTrack {
                    position: e.position,
                    track_id: e.entity_id,
                    track_title: e.name,
                    artist_id: e.artist_id,
                    artist_name: e.artist_name,
                    album_image: e.image,
                    listeners: whole(e.listeners),
                    plays: whole(e.weight),
                })
                .collect(),
        })
        .into_response()
    })
    .await
}

pub fn _track_ranking_doc(op: TransformOperation) -> TransformOperation {
    document(op, "Global track ranking", "tracks").response_with::<200, Json<TrackRanking>, _>(
        |r| caching_headers(r).description("A page of the ranking"),
    )
}

async fn serve(
    state: &AppState,
    kind: Kind,
    q: RankingQuery,
    headers: &HeaderMap,
    body: impl FnOnce(RankingInfo, Vec<ServedEntry>) -> Response,
) -> ApiResult<Response> {
    let period = match q.period.as_deref() {
        None => Period::Week,
        Some(p) => Period::parse(p).ok_or_else(|| {
            AppError::BadRequest(format!("unknown period `{p}`: week, month or year"))
        })?,
    };
    let offset = q.offset.unwrap_or(0);
    if offset < 0 {
        return Err(AppError::BadRequest("offset can't be negative".into()));
    }
    let limit = super::page_limit(q.limit, DEFAULT_LIMIT, MAX_LIMIT);
    let min_listeners = state.rankings.min_listeners;
    let now = Utc::now();

    if let Some(tags) = headers.get(header::IF_NONE_MATCH)
        && let Some(version) = rankings_db::snapshot_version(&state.db, period, kind).await?
    {
        let etag = etag(period, version, min_listeners, now);
        if caching::matches(tags, &etag) {
            return Ok(with_caching(StatusCode::NOT_MODIFIED.into_response(), etag));
        }
    }

    let min_listeners_thousandths = i64::from(min_listeners) * FULL;
    let Some(page) = rankings_db::served_page(
        &state.db,
        period,
        kind,
        min_listeners_thousandths,
        offset,
        limit,
    )
    .await?
    else {
        let error = ErrorBody {
            error: format!("no {} ranking has been computed yet", period.as_str()),
        };
        let mut response = (StatusCode::NOT_FOUND, Json(error)).into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL_MISSING),
        );
        return Ok(response);
    };
    let v = page.version;
    let info = RankingInfo {
        period,
        from: v.from_day,
        to: v.to_day,
        computed_at: v.computed_at,
        stale: is_stale(period, v, now),
        total: page.total,
        offset,
        limit,
    };
    let etag = etag(period, v, min_listeners, now);
    Ok(with_caching(body(info, page.entries), etag))
}

fn is_stale(period: Period, v: SnapshotVersion, now: DateTime<Utc>) -> bool {
    now > period.stale_after(v.to_day, v.computed_at)
}

/// Weak, since compression changes the bytes. Whatever changes the body for
/// the same URL is in it: the snapshot, the minimum and staleness.
fn etag(period: Period, v: SnapshotVersion, min_listeners: u32, now: DateTime<Utc>) -> String {
    format!(
        "W/\"r1-{}-{min_listeners}-{}\"",
        v.computed_at.timestamp_micros(),
        if is_stale(period, v, now) { "s" } else { "f" }
    )
}

fn with_caching(mut response: Response, etag: String) -> Response {
    let headers = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    response
}

fn document<'t>(op: TransformOperation<'t>, summary: &str, things: &str) -> TransformOperation<'t> {
    op.summary(summary)
        .description(&format!(
            "The most listened {things} of the last week, month or year (rolling UTC days \
             ending today), ranked by listeners, then by plays. Each account counts once at \
             most, and less when its plays don't look like real listening; plays are weighed \
             the same way. No account is named, and {things} with fewer listeners than the \
             server's minimum (5 unless configured) are left out, so the ranking may be \
             shorter than 1,000 positions.\n\n\
             The server recomputes week and month rankings every hour and the year's every \
             day: a page says when its ranking was computed (`computed_at`) and which days it \
             covers (`from`, `to`). When a recomputation is overdue, the last ranking is \
             still served, with `stale: true`; before the first one, 404.\n\n\
             Pages carry an `ETag` and may be cached for 5 minutes; send the tag back in \
             `If-None-Match` to get a 304 while the ranking hasn't changed."
        ))
        .tag("Rankings")
        .with(|op| {
            caching::if_none_match(
                op,
                "The `ETag` of a page fetched before: 304 if the ranking hasn't changed since",
            )
        })
        .response_with::<304, (), _>(|r| {
            caching_headers(r).description("Not modified since the `ETag` in `If-None-Match`")
        })
        .response_with::<400, ErrorJson, _>(|r| r.description("Unknown period or negative offset"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("The server hasn't computed a ranking for this period yet")
        })
}

fn caching_headers<T>(r: TransformResponse<'_, T>) -> TransformResponse<'_, T> {
    caching::headers(
        r,
        "The ranking's version, for `If-None-Match`",
        CACHE_CONTROL,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use crate::test_app::{TestApp, UPLOADS_URL, with_app};
    use db::queries::rankings::{self as rankings_db, Kind, Period};
    use shared::ranking::{self, RankingParams};

    /// 250 artists, `Artist {n}` with one track `Song {n}` on an album, each
    /// with an uploaded image.
    async fn catalog(pool: &PgPool) {
        for sql in [
            "INSERT INTO artists (name, name_normalized, image_url)
             SELECT 'Artist ' || n, 'artist ' || n, 'artists/ab/' || lpad(n::text, 32, '0') || '.jpg'
             FROM generate_series(1, 250) n",
            "INSERT INTO albums (artist_id, title, title_normalized, image_url)
             SELECT id, 'Album', 'album', replace(image_url, 'artists/', 'albums/') FROM artists",
            "INSERT INTO tracks (artist_id, album_id, title, title_normalized)
             SELECT a.id, al.id, replace(a.name, 'Artist', 'Song'), replace(a.name_normalized, 'artist', 'song')
             FROM artists a JOIN albums al ON al.artist_id = a.id",
        ] {
            sqlx::query(sql).execute(pool).await.unwrap();
        }
    }

    /// Stores `period`'s rankings as computed at `computed_at`: position `n`
    /// is artist (track) `n`, credited with 300 - n listeners and twice the
    /// plays (plus fractions that round down), down to position 239;
    /// position 240 with exactly 5 listeners and 241 to 250 with 4.999.
    async fn store(pool: &PgPool, period: Period, computed_at: DateTime<Utc>) {
        let ruleset = rankings_db::register_ruleset(pool, &RankingParams::default())
            .await
            .unwrap()
            .id;
        let (from, to) = period.range(computed_at.date_naive());
        for kind in Kind::ALL {
            sqlx::query("DELETE FROM ranking_snapshots WHERE period = $1 AND kind = $2")
                .bind(period.as_str())
                .bind(kind.as_str())
                .execute(pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO ranking_snapshots
                     (period, kind, from_day, to_day, ruleset_id, ranked, pending_days,
                      computed_at, took_ms)
                 VALUES ($1, $2, $3, $4, $5, 250, 0, $6, 1)",
            )
            .bind(period.as_str())
            .bind(kind.as_str())
            .bind(from)
            .bind(to)
            .bind(ruleset)
            .bind(computed_at)
            .execute(pool)
            .await
            .unwrap();
            let entity = match kind {
                Kind::Artist => "SELECT id FROM artists WHERE name = 'Artist ' || n",
                Kind::Track => "SELECT id FROM tracks WHERE title = 'Song ' || n",
            };
            sqlx::query(&format!(
                "INSERT INTO ranking_entries
                     (period, kind, position, entity_id, listeners, weight, raw_listeners, raw_plays)
                 SELECT $1, $2, n, ({entity}),
                        CASE WHEN n < 240 THEN (300 - n) * 1000 + 400
                             WHEN n = 240 THEN 5000 ELSE 4999 END,
                        CASE WHEN n < 240 THEN (300 - n) * 2000 + 499 ELSE 5000 END,
                        777, 7777
                 FROM generate_series(1, 250) n"
            ))
            .bind(period.as_str())
            .bind(kind.as_str())
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn get(
        app: &TestApp,
        uri: &str,
        if_none_match: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::get(uri);
        if let Some(tag) = if_none_match {
            req = req.header(header::IF_NONE_MATCH, tag);
        }
        let (status, headers, body) = app.send(req.body(Body::empty()).unwrap()).await;
        (
            status,
            headers,
            serde_json::from_str(&body).unwrap_or(Value::Null),
        )
    }

    fn keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn positions(page: &Value) -> Vec<i64> {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["position"].as_i64().unwrap())
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn pages_of_the_stored_rankings() {
        with_app(Default::default(), |app| async move {
            // Before the worker's first snapshot.
            let (status, headers, body) = get(&app, "/v1/rankings/artists", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(body["error"], "no week ranking has been computed yet");
            assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=60");

            catalog(&app.pool).await;
            let now = Utc::now();
            store(&app.pool, Period::Week, now).await;

            let (status, headers, page) = get(&app, "/v1/rankings/artists", None).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(
                headers[header::CACHE_CONTROL],
                "public, max-age=300, stale-while-revalidate=300"
            );
            assert!(headers[header::ETAG].to_str().unwrap().starts_with("W/\""));
            let today = now.date_naive();
            assert_eq!(page["period"], "week");
            assert_eq!(page["from"], (today - TimeDelta::days(6)).to_string());
            assert_eq!(page["to"], today.to_string());
            let computed_at: DateTime<Utc> = page["computed_at"].as_str().unwrap().parse().unwrap();
            assert!((computed_at - now).abs() < TimeDelta::milliseconds(1));
            assert_eq!(page["stale"], false);
            assert_eq!(page["total"], 240);
            assert_eq!(
                (page["offset"].clone(), page["limit"].clone()),
                (json!(0), json!(50))
            );
            assert_eq!(positions(&page), (1..=50).collect::<Vec<_>>());
            // Nothing but the entity and its two numbers: no raw counts.
            let first = &page["entries"][0];
            assert_eq!(
                keys(first),
                BTreeSet::from([
                    "position",
                    "artist_id",
                    "artist_name",
                    "image_url",
                    "listeners",
                    "plays"
                ])
            );
            assert_eq!(first["artist_name"], "Artist 1");
            assert_eq!(
                first["image_url"],
                format!("{UPLOADS_URL}/artists/ab/{:032}.jpg", 1)
            );
            assert_eq!(
                (first["listeners"].clone(), first["plays"].clone()),
                (json!(299), json!(598))
            );

            // Pages are bounded; the last served position is the last with
            // 5 listeners, however its 4.999 neighbours would round.
            let (_, _, page) = get(&app, "/v1/rankings/artists?limit=1000", None).await;
            assert_eq!(
                (page["limit"].clone(), positions(&page).len()),
                (json!(200), 200)
            );
            for limit in ["0", "-5"] {
                let (_, _, page) =
                    get(&app, &format!("/v1/rankings/artists?limit={limit}"), None).await;
                assert_eq!(positions(&page), vec![1], "{limit}");
            }
            let (_, _, page) = get(&app, "/v1/rankings/artists?offset=230&limit=50", None).await;
            assert_eq!(positions(&page), (231..=240).collect::<Vec<_>>());
            assert_eq!(page["entries"][9]["listeners"], 5);
            assert_eq!(page["total"], 240);
            for offset in ["240", "100000000000"] {
                let (status, _, page) =
                    get(&app, &format!("/v1/rankings/artists?offset={offset}"), None).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(page["entries"], json!([]), "{offset}");
            }

            // Unknown periods and kinds, negative offsets, periods not
            // computed yet.
            let (status, _, body) = get(&app, "/v1/rankings/artists?offset=-1", None).await;
            assert_eq!(
                (status, body["error"].clone()),
                (
                    StatusCode::BAD_REQUEST,
                    json!("bad request: offset can't be negative")
                )
            );
            let (status, _, body) = get(&app, "/v1/rankings/artists?period=decade", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                body["error"].as_str().unwrap().contains("`decade`"),
                "{body}"
            );
            let (status, _, _) = get(&app, "/v1/rankings/tracks?period=month", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = get(&app, "/v1/rankings/albums", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let (status, _, page) =
                get(&app, "/v1/rankings/tracks?period=week&limit=2", None).await;
            assert_eq!(status, StatusCode::OK);
            let first = &page["entries"][0];
            assert_eq!(
                keys(first),
                BTreeSet::from([
                    "position",
                    "track_id",
                    "track_title",
                    "artist_id",
                    "artist_name",
                    "album_image",
                    "listeners",
                    "plays"
                ])
            );
            assert_eq!(first["track_title"], "Song 1");
            assert_eq!(first["artist_name"], "Artist 1");
            assert_eq!(
                first["album_image"],
                format!("{UPLOADS_URL}/albums/ab/{:032}.jpg", 1)
            );
            assert_eq!(page["total"], 240);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn conditional_requests_and_stale_rankings() {
        with_app(Default::default(), |app| async move {
            catalog(&app.pool).await;
            store(&app.pool, Period::Week, Utc::now()).await;
            let uri = "/v1/rankings/tracks";
            let (_, headers, page) = get(&app, uri, None).await;
            let etag = headers[header::ETAG].to_str().unwrap().to_owned();

            for tag in [etag.as_str(), etag.trim_start_matches("W/"), "*"] {
                let (status, headers, body) = get(&app, uri, Some(tag)).await;
                assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
                assert_eq!(headers[header::ETAG], etag.as_str());
                assert_eq!(
                    headers[header::CACHE_CONTROL],
                    "public, max-age=300, stale-while-revalidate=300"
                );
                assert_eq!(body, Value::Null);
            }
            let (status, _, again) = get(&app, uri, Some("W/\"other\"")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(again, page);

            // Another snapshot, three hours old: stale, so served with a new
            // tag that says so.
            store(&app.pool, Period::Week, Utc::now() - TimeDelta::hours(3)).await;
            let (status, headers, page) = get(&app, uri, Some(&etag)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["stale"], true);
            let stale = headers[header::ETAG].to_str().unwrap().to_owned();
            assert_ne!(stale, etag);
            let (status, _, _) = get(&app, uri, Some(&stale)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);

            // No snapshot, no 304.
            let (status, _, _) = get(&app, "/v1/rankings/tracks?period=year", Some("*")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// Users, each weighing 1 (a full listener) for some artists today, and
    /// the week's snapshot computed from them as the worker does.
    async fn listen(pool: &PgPool, listeners: &[(&str, bool, &[&str])]) {
        let ruleset = rankings_db::register_ruleset(pool, &RankingParams::default())
            .await
            .unwrap();
        for (username, private, artists) in listeners {
            let user_id: i64 = sqlx::query_scalar(
                "INSERT INTO users (username, email, password_hash, is_private)
                 VALUES ($1, $1 || '@test', 'x', $2) RETURNING id",
            )
            .bind(username)
            .bind(private)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO ranking_days (user_id, day, ruleset_id, classified_at, plays,
                     weighted, weight, unclassified, suspect, duplicate, no_data, imported,
                     new_account, short_listen, no_listened, unknown_client, track_cap, artist_cap)
                 VALUES ($1, CURRENT_DATE, $2, NOW(), 1, 1, 1000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)",
            )
            .bind(user_id)
            .bind(ruleset.id)
            .execute(pool)
            .await
            .unwrap();
            for artist in *artists {
                sqlx::query(
                    "WITH a AS (
                         INSERT INTO artists (name, name_normalized) VALUES ($2, lower($2))
                         ON CONFLICT (name_normalized) DO UPDATE SET name = EXCLUDED.name
                         RETURNING id
                     ), t AS (
                         INSERT INTO tracks (artist_id, title, title_normalized)
                         SELECT id, 'Song', 'song' FROM a
                         ON CONFLICT (artist_id, title_normalized) DO UPDATE SET title = EXCLUDED.title
                         RETURNING id, artist_id
                     )
                     INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
                     SELECT $1, CURRENT_DATE, id, artist_id, 1, 1000 FROM t",
                )
                .bind(user_id)
                .bind(artist)
                .execute(pool)
                .await
                .unwrap();
            }
        }
        let today = Utc::now().date_naive();
        rankings_db::refresh_snapshots(pool, &ruleset, Period::Week, today, 1_000)
            .await
            .unwrap();
    }

    /// A private profile's listening counts like anyone's, and no page names
    /// a listener; but an entry fewer than 5 accounts carry, private or not,
    /// isn't served at all.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn private_listeners_count_unnamed_and_small_groups_stay_hidden() {
        with_app(Default::default(), |app| async move {
            let crowd: &[&str] = &["Crowd", "Quartet"];
            listen(
                &app.pool,
                &[
                    ("fan1", false, crowd),
                    ("fan2", false, crowd),
                    ("fan3", false, crowd),
                    ("fan4", false, crowd),
                    ("hermit", true, &["Crowd", "Hidden Gem"]),
                ],
            )
            .await;
            let (status, _, body) = app.get("/v1/rankings/artists").await;
            assert_eq!(status, StatusCode::OK);
            let page: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(page["total"], 1, "{page}");
            assert_eq!(page["entries"][0]["artist_name"], "Crowd");
            assert_eq!(page["entries"][0]["listeners"], 5);
            for name in ["hermit", "fan1", "Hidden Gem", "Quartet"] {
                assert!(!body.contains(name), "{name} in {body}");
            }
            // The worker stored all three: only the routes leave two out.
            let stored =
                rankings_db::snapshot_entries(&app.pool, Period::Week, Kind::Artist, 0, 10)
                    .await
                    .unwrap();
            assert_eq!(stored.len(), 3);
        })
        .await;
    }

    /// The routes serve the stored ranking `worker rank top` prints, rounded,
    /// down to the cutoff it marks, through every page.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn the_routes_serve_what_rank_top_prints() {
        with_app(Default::default(), |app| async move {
            // Fan k listens to bands 1 to 12 - k, so band n has 12 - n fans.
            let names: Vec<String> = (1..=11).map(|n| format!("Band {n}")).collect();
            let fans: Vec<(String, Vec<&str>)> = (1..=11)
                .map(|k| {
                    let liked = names[..12 - k].iter().map(String::as_str).collect();
                    (format!("fan{k}"), liked)
                })
                .collect();
            let listeners: Vec<(&str, bool, &[&str])> = fans
                .iter()
                .map(|(name, liked)| (name.as_str(), false, liked.as_slice()))
                .collect();
            listen(&app.pool, &listeners).await;

            let min = crate::handlers::rankings::RankingConfig::default().min_listeners;
            for kind in Kind::ALL {
                let printed =
                    rankings_db::snapshot_entries(&app.pool, Period::Week, kind, 0, 1_000)
                        .await
                        .unwrap();
                let expected: Vec<Value> = printed
                    .iter()
                    .take_while(|e| ranking::is_served(e.listeners, min))
                    .map(|e| {
                        json!([
                            e.position,
                            e.entity_id,
                            e.name,
                            e.artist_name.clone().or(e.name.clone()),
                            ranking::whole(e.listeners),
                            ranking::whole(e.weight)
                        ])
                    })
                    .collect();
                assert!(
                    expected.len() >= 3 && expected.len() < printed.len(),
                    "{printed:?}"
                );

                let mut served = Vec::new();
                let mut offset = 0;
                loop {
                    let uri = format!("/v1/rankings/{}s?limit=3&offset={offset}", kind.as_str());
                    let (_, _, page) = get(&app, &uri, None).await;
                    assert_eq!(page["total"], expected.len() as i64);
                    let entries = page["entries"].as_array().unwrap().clone();
                    if entries.is_empty() {
                        break;
                    }
                    for e in &entries {
                        let (id, name) = match kind {
                            Kind::Artist => (&e["artist_id"], &e["artist_name"]),
                            Kind::Track => (&e["track_id"], &e["track_title"]),
                        };
                        served.push(json!([
                            e["position"],
                            id,
                            name,
                            e["artist_name"],
                            e["listeners"],
                            e["plays"]
                        ]));
                    }
                    offset += 3;
                }
                assert_eq!(served, expected, "{kind:?}");
            }
        })
        .await;
    }
}
