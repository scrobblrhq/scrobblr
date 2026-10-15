//! Tags (`db::queries::tags`; docs/social.md, Tags): the counts everyone
//! sees, and an account's own tags.

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{IdPath, IdTagPath, TagPath, UsernamePath, caching};
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::tags::{self as tags_db, Added, TagSource};
use db::queries::{bios as bios_db, tracks as tracks_db, users as users_db};
use shared::community::{self, TagError};
use shared::cursor::Cursor;
use shared::models::{
    CatalogKind, EntityTags, TaggedEntityPage, TopTag, TopTags, UserTagPage, UserTags,
};

const CACHE_CONTROL: &str = "public, max-age=300";
const TOP_TAGS: i64 = 100;
const USER_TAGS: i64 = 200;

pub async fn ensure_entity(state: &AppState, kind: CatalogKind, id: i64) -> ApiResult<()> {
    let exists = match kind {
        CatalogKind::Artist => tracks_db::find_artist_by_id(&state.db, id).await?.is_some(),
        CatalogKind::Album => tracks_db::find_album_by_id(&state.db, id).await?.is_some(),
        CatalogKind::Track => tracks_db::find_track_by_id(&state.db, id).await?.is_some(),
    };
    exists.then_some(()).ok_or(AppError::NotFound)
}

fn noun(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Artist => "artist",
        CatalogKind::Album => "album",
        CatalogKind::Track => "track",
    }
}

fn min_weight(state: &AppState) -> i64 {
    i64::from(state.rankings.min_listeners) * shared::ranking::FULL
}

const COUNTED: &str = "Each account counts once, weighed as the rankings weigh listeners: in full from a week after registering, at its factor when the anti-botting coordination detection flagged it, not at all while suspended or waiting to be deleted; private accounts count, unnamed. A tag is shown on an entity from the server's minimum of weighted accounts (`RANKING_MIN_LISTENERS`, 5 unless configured), so nobody can put one there alone. The worker recounts an entity within a minute of a change, and every entity every 6 hours. Tags feed nothing else: rankings, recommendations and similar artists don't read them.";

async fn top_tags(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    ensure_entity(state, kind, id).await?;
    let tags = tags_db::top_tags(&state.db, kind, id, min_weight(state), TOP_TAGS)
        .await?
        .into_iter()
        .map(|(name, weight)| TopTag {
            name,
            taggers: shared::ranking::whole(weight),
        })
        .collect();
    let body = TopTags {
        entity_type: kind,
        entity_id: id,
        tags,
    };
    Ok(caching::json_tagged(headers, &body, CACHE_CONTROL))
}

/// GET /v1/artist/:id/tags
pub async fn artist_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    top_tags(&state, CatalogKind::Artist, id, &headers).await
}

/// GET /v1/album/:id/tags
pub async fn album_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    top_tags(&state, CatalogKind::Album, id, &headers).await
}

/// GET /v1/track/:id/tags
pub async fn track_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    top_tags(&state, CatalogKind::Track, id, &headers).await
}

fn cached<'t>(op: TransformOperation<'t>) -> TransformOperation<'t> {
    op.with(|op| {
        caching::if_none_match(
            op,
            "The `ETag` of a response fetched before: 304 if it hasn't changed since",
        )
    })
    .response_with::<304, (), _>(|r| {
        caching::headers(r, "A tag of the body", CACHE_CONTROL)
            .description("Not modified since the `ETag` in `If-None-Match`")
    })
}

fn top_tags_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    let noun = noun(kind);
    cached(op)
        .summary(match kind {
            CatalogKind::Artist => "An artist's tags",
            CatalogKind::Album => "An album's tags",
            CatalogKind::Track => "A track's tags",
        })
        .description(&format!("The {noun}'s tags, most taggers first (100 at most). {COUNTED} Cacheable for 5 minutes, with an `ETag`."))
        .tag("Tags")
        .response_with::<200, Json<TopTags>, _>(|r| {
            caching::headers(r, "A tag of the body, for `If-None-Match`", CACHE_CONTROL)
        })
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("No such {noun}")))
}

pub fn _artist_tags_doc(op: TransformOperation) -> TransformOperation {
    top_tags_doc(op, CatalogKind::Artist)
}

pub fn _album_tags_doc(op: TransformOperation) -> TransformOperation {
    top_tags_doc(op, CatalogKind::Album)
}

pub fn _track_tags_doc(op: TransformOperation) -> TransformOperation {
    top_tags_doc(op, CatalogKind::Track)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaggedQuery {
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// Positions to skip, 0 by default.
    pub offset: Option<i64>,
}

fn tag_param(raw: &str) -> ApiResult<String> {
    community::tag_name(raw).map_err(|e| AppError::BadRequest(e.to_string()))
}

async fn tagged(
    state: &AppState,
    kind: CatalogKind,
    raw: &str,
    q: &TaggedQuery,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let tag = tag_param(raw)?;
    let offset = q.offset.unwrap_or(0);
    if offset < 0 {
        return Err(AppError::BadRequest("offset can't be negative".into()));
    }
    let limit = super::page_limit(q.limit, 50, 200);
    let tag_id = tags_db::tag_id(&state.db, &tag)
        .await?
        .ok_or(AppError::NotFound)?;
    let (entries, total) =
        tags_db::top_entities(&state.db, tag_id, kind, min_weight(state), limit, offset).await?;
    let body = TaggedEntityPage {
        tag,
        entries,
        total,
    };
    Ok(caching::json_tagged(headers, &body, CACHE_CONTROL))
}

/// GET /v1/tag/:tag/artists
pub async fn tagged_artists(
    State(state): State<AppState>,
    Path(TagPath { tag }): Path<TagPath>,
    Query(q): Query<TaggedQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    tagged(&state, CatalogKind::Artist, &tag, &q, &headers).await
}

/// GET /v1/tag/:tag/albums
pub async fn tagged_albums(
    State(state): State<AppState>,
    Path(TagPath { tag }): Path<TagPath>,
    Query(q): Query<TaggedQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    tagged(&state, CatalogKind::Album, &tag, &q, &headers).await
}

/// GET /v1/tag/:tag/tracks
pub async fn tagged_tracks(
    State(state): State<AppState>,
    Path(TagPath { tag }): Path<TagPath>,
    Query(q): Query<TaggedQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    tagged(&state, CatalogKind::Track, &tag, &q, &headers).await
}

fn tagged_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    let nouns = format!("{}s", noun(kind));
    cached(op)
        .summary(&format!("The {nouns} a tag is on"))
        .description(&format!("The {nouns} the tag is shown on, most taggers first, `limit` a page (50 by default, 200 at most) from `offset`; `total` counts them all. The tag is normalized as tags are (case, spacing and Unicode forms don't matter). {COUNTED} Cacheable for 5 minutes, with an `ETag`."))
        .tag("Tags")
        .response_with::<200, Json<TaggedEntityPage>, _>(|r| {
            caching::headers(r, "A tag of the body, for `If-None-Match`", CACHE_CONTROL)
        })
        .response_with::<400, ErrorJson, _>(|r| r.description("Not a valid tag, or a negative offset"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Nobody uses that tag"))
}

pub fn _tagged_artists_doc(op: TransformOperation) -> TransformOperation {
    tagged_doc(op, CatalogKind::Artist)
}

pub fn _tagged_albums_doc(op: TransformOperation) -> TransformOperation {
    tagged_doc(op, CatalogKind::Album)
}

pub fn _tagged_tracks_doc(op: TransformOperation) -> TransformOperation {
    tagged_doc(op, CatalogKind::Track)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddTagsRequest {
    /// Up to 10 tags, each 1 to 40 letters, digits, spaces and `- & ' . + ! _`
    /// (normalized: lowercase, NFKC, single spaces).
    pub tags: Vec<String>,
}

/// Whether the account may tag now (not suspended), within its hourly
/// limits; tags of new or flagged accounts are kept but weigh nothing or
/// less.
pub async fn may_tag(state: &AppState, user_id: i64, ip: &str) -> ApiResult<()> {
    if !limits::tag_attempt(state, user_id, ip).await {
        return Err(AppError::RateLimited);
    }
    let editor = bios_db::editor(&state.db, user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if editor.suspended || editor.leaving {
        return Err(AppError::NotAllowed("this account can't tag now".into()));
    }
    Ok(())
}

/// Normalizes `raw` and refuses blocked or invalid tags.
pub async fn tag_names(state: &AppState, raw: &[String]) -> ApiResult<Vec<String>> {
    if raw.is_empty() || raw.len() as i64 > community::MAX_TAGS_PER_ENTITY {
        return Err(AppError::BadRequest(format!(
            "send 1 to {} tags",
            community::MAX_TAGS_PER_ENTITY
        )));
    }
    let names = raw
        .iter()
        .map(|t| community::tag_name(t).map_err(|e| AppError::BadRequest(format!("`{t}`: {e}"))))
        .collect::<ApiResult<Vec<_>>>()?;
    let blocked = tags_db::blocklist(&state.db).await?;
    if let Some(name) = names.iter().find(|n| community::is_blocked(n, &blocked)) {
        return Err(AppError::BadRequest(
            TagError::Blocked(name.clone()).to_string(),
        ));
    }
    Ok(names)
}

pub fn added_error(added: Added) -> AppError {
    AppError::Conflict(match added {
        Added::TooManyHere => format!(
            "you may put at most {} tags on one artist, album or track",
            community::MAX_TAGS_PER_ENTITY
        ),
        Added::EntityFull => format!(
            "this already carries {} different tags, the most allowed: use one of them",
            community::MAX_ENTITY_TAGS
        ),
        Added::AtLimit | Added::Tags(_) => format!(
            "you already hold {} tags, the most allowed",
            community::MAX_TAGGINGS
        ),
    })
}

async fn add(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    user_id: i64,
    ip: &str,
    req: AddTagsRequest,
) -> ApiResult<Json<EntityTags>> {
    ensure_entity(state, kind, id).await?;
    may_tag(state, user_id, ip).await?;
    let names = tag_names(state, &req.tags).await?;
    match tags_db::add(&state.db, user_id, kind, id, &names, TagSource::NATIVE).await? {
        Added::Tags(tags) => Ok(Json(EntityTags {
            entity_type: kind,
            entity_id: id,
            tags,
        })),
        refused => Err(added_error(refused)),
    }
}

/// POST /v1/artist/:id/tags
pub async fn tag_artist(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<AddTagsRequest>,
) -> ApiResult<Json<EntityTags>> {
    add(&state, CatalogKind::Artist, id, auth_user.id, &ip, req).await
}

/// POST /v1/album/:id/tags
pub async fn tag_album(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<AddTagsRequest>,
) -> ApiResult<Json<EntityTags>> {
    add(&state, CatalogKind::Album, id, auth_user.id, &ip, req).await
}

/// POST /v1/track/:id/tags
pub async fn tag_track(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<AddTagsRequest>,
) -> ApiResult<Json<EntityTags>> {
    add(&state, CatalogKind::Track, id, auth_user.id, &ip, req).await
}

fn tag_limits() -> String {
    format!(
        "Over {} tag changes an hour for this account or {} from this address, or over the request limit for this address",
        limits::TAG_CHANGES_PER_USER,
        limits::TAG_CHANGES_PER_IP
    )
}

fn add_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    let noun = noun(kind);
    op.summary(&format!("Tag the {noun}"))
        .description(&format!("Adds tags to the user's own on the {noun} and returns them all; tags already there change nothing. Tags are normalized (lowercase, NFKC, single spaces), 1 to 40 letters, digits, spaces and `- & ' . + ! _`; a term the operator blocked refuses the request. An account puts at most {} tags on one artist, album or track and holds {} in all, and an entity carries {} different tags at most (409). A suspended account can't tag (403); a new one may, its tags weighing nothing in the counts for a week. Adding and removing count together against {} changes an hour per account, {} per address, through this API and the Last.fm-compatible one.", community::MAX_TAGS_PER_ENTITY, community::MAX_TAGGINGS, community::MAX_ENTITY_TAGS, limits::TAG_CHANGES_PER_USER, limits::TAG_CHANGES_PER_IP))
        .tag("Tags")
        .response::<200, Json<EntityTags>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("No tags or more than 10, an invalid tag, or a blocked one"))
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("No such {noun}")))
        .response_with::<409, ErrorJson, _>(|r| r.description("Past a cap"))
        .response_with::<429, ErrorJson, _>(|r| r.description(&tag_limits()))
}

pub fn _tag_artist_doc(op: TransformOperation) -> TransformOperation {
    add_doc(op, CatalogKind::Artist)
}

pub fn _tag_album_doc(op: TransformOperation) -> TransformOperation {
    add_doc(op, CatalogKind::Album)
}

pub fn _tag_track_doc(op: TransformOperation) -> TransformOperation {
    add_doc(op, CatalogKind::Track)
}

async fn untag(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    raw: &str,
    user_id: i64,
    ip: &str,
) -> ApiResult<StatusCode> {
    if !limits::tag_attempt(state, user_id, ip).await {
        return Err(AppError::RateLimited);
    }
    let tag = tag_param(raw)?;
    if !tags_db::remove(&state.db, user_id, kind, id, &tag).await? {
        ensure_entity(state, kind, id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /v1/artist/:id/tags/:tag
pub async fn untag_artist(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdTagPath { id, tag }): Path<IdTagPath>,
) -> ApiResult<StatusCode> {
    untag(&state, CatalogKind::Artist, id, &tag, auth_user.id, &ip).await
}

/// DELETE /v1/album/:id/tags/:tag
pub async fn untag_album(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdTagPath { id, tag }): Path<IdTagPath>,
) -> ApiResult<StatusCode> {
    untag(&state, CatalogKind::Album, id, &tag, auth_user.id, &ip).await
}

/// DELETE /v1/track/:id/tags/:tag
pub async fn untag_track(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdTagPath { id, tag }): Path<IdTagPath>,
) -> ApiResult<StatusCode> {
    untag(&state, CatalogKind::Track, id, &tag, auth_user.id, &ip).await
}

fn untag_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    let noun = noun(kind);
    op.summary(&format!("Untag the {noun}"))
        .description(&format!("Takes the tag off the user's own on the {noun}; removing one not there changes nothing. Counts against the same hourly limits as adding."))
        .tag("Tags")
        .response_with::<204, (), _>(|r| r.description("Not among the user's tags any more"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Not a valid tag"))
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("No such {noun}")))
        .response_with::<429, ErrorJson, _>(|r| r.description(&tag_limits()))
}

pub fn _untag_artist_doc(op: TransformOperation) -> TransformOperation {
    untag_doc(op, CatalogKind::Artist)
}

pub fn _untag_album_doc(op: TransformOperation) -> TransformOperation {
    untag_doc(op, CatalogKind::Album)
}

pub fn _untag_track_doc(op: TransformOperation) -> TransformOperation {
    untag_doc(op, CatalogKind::Track)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MyTagsQuery {
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// The previous page's `next`, for the page after it.
    pub before: Option<String>,
    /// Only the tags on this artist (or album, or track: one of the three).
    pub artist: Option<i64>,
    pub album: Option<i64>,
    pub track: Option<i64>,
}

/// GET /v1/user/me/tags
pub async fn my_tags(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<MyTagsQuery>,
) -> ApiResult<Json<UserTagPage>> {
    let on = match (q.artist, q.album, q.track) {
        (None, None, None) => None,
        (Some(id), None, None) => Some((CatalogKind::Artist, id)),
        (None, Some(id), None) => Some((CatalogKind::Album, id)),
        (None, None, Some(id)) => Some((CatalogKind::Track, id)),
        _ => {
            return Err(AppError::BadRequest(
                "name one of artist, album and track at most".into(),
            ));
        }
    };
    let before = q
        .before
        .as_deref()
        .map(|c| {
            Cursor::parse(c).ok_or_else(|| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let limit = super::page_limit(q.limit, 50, 200);
    let mut page = tags_db::user_page(&state.db, auth_user.id, on, before, limit + 1).await?;
    let next = if page.len() as i64 > limit {
        page.truncate(limit as usize);
        page.last().map(|(_, cursor)| cursor.encode())
    } else {
        None
    };
    Ok(Json(UserTagPage {
        entries: page.into_iter().map(|(tag, _)| tag).collect(),
        next,
    }))
}

pub fn _my_tags_doc(op: TransformOperation) -> TransformOperation {
    op.summary("My tags")
        .description("The signed-in user's tags, newest first, each with what it is on: `limit` a page (50 by default, 200 at most), `next` as `before` for the following page (`null` on the last). `artist`, `album` or `track` keeps those on one entity, as its page shows them.")
        .tag("Tags")
        .response::<200, Json<UserTagPage>>()
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("Invalid `before` cursor, or more than one entity named")
        })
}

/// GET /v1/user/:username/tags
pub async fn user_tags(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<UserTags>> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer, &user).await?;
    Ok(Json(UserTags {
        tags: tags_db::user_counts(&state.db, user.id, USER_TAGS).await?,
    }))
}

pub fn _user_tags_doc(op: TransformOperation) -> TransformOperation {
    op.summary("A user's tags")
        .description("The tags the user uses, with how many artists, albums and tracks each is on, most used first (200 at most). As visible as the profile: a private profile answers 403 to everyone but its owner, and an account waiting to be deleted or blocking the signed-in viewer 404, as one that doesn't exist.")
        .tag("Tags")
        .response::<200, Json<UserTags>>()
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, waiting to be deleted, or blocking the viewer")
        })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use chrono::Utc;
    use serde_json::{Value, json};

    use crate::compat::CompatConfig;
    use crate::test_app::{TestApp, with_app};
    use db::queries::tags::{self as tags_db, Weighing};

    async fn band(app: &TestApp) -> i64 {
        db::queries::tracks::find_or_create_artist(&app.pool, "Band")
            .await
            .unwrap()
            .id
    }

    async fn recount(app: &TestApp) {
        sqlx::query("UPDATE tag_queue SET due_at = NOW()")
            .execute(&app.pool)
            .await
            .unwrap();
        tags_db::recount_due(&app.pool, Weighing::at(Utc::now()), 100)
            .await
            .unwrap();
    }

    /// Tags arrive normalized, duplicates once; invalid, blocked or too
    /// many are refused; removing takes any spelling of the tag.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn tags_are_normalized_and_checked() {
        with_app(CompatConfig::default(), |app| async move {
            let band = band(&app).await;
            let path = format!("/v1/artist/{band}/tags");
            let (status, mine) = app
                .api(
                    Method::POST,
                    &path,
                    Some(json!({ "tags": [" Post   Rock ", "POST ROCK", "Ｊ－ｐｏｐ", "E\u{0301}lectro"] })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{mine}");
            assert_eq!(mine["tags"], json!(["j-pop", "post rock", "électro"]));

            for bad in [json!({ "tags": ["a/b"] }), json!({ "tags": [] }), json!({ "tags": ["x\u{202E}"] })] {
                let (status, _) = app.api(Method::POST, &path, Some(bad.clone())).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            }
            let eleven: Vec<String> = (0..11).map(|i| format!("t{i}")).collect();
            let (status, _) = app.api(Method::POST, &path, Some(json!({ "tags": eleven }))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let eight: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
            let (status, body) = app.api(Method::POST, &path, Some(json!({ "tags": eight }))).await;
            assert_eq!(status, StatusCode::CONFLICT, "3 + 8 > 10: {body}");

            sqlx::query("INSERT INTO tag_blocklist (term) VALUES ('spam')")
                .execute(&app.pool)
                .await
                .unwrap();
            let (status, body) = app
                .api(Method::POST, &path, Some(json!({ "tags": ["Total SPAM here"] })))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"], "bad request: the tag `total spam here` isn't allowed");

            let (status, _) = app
                .api(Method::DELETE, &format!("{path}/POST%20%20Rock"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let (_, mine) = app
                .api(Method::GET, &format!("/v1/user/me/tags?artist={band}"), None)
                .await;
            let tags: Vec<&str> = mine["entries"].as_array().unwrap().iter().map(|e| e["tag"].as_str().unwrap()).collect();
            assert_eq!(tags.len(), 2);
            assert!(!tags.contains(&"post rock"));
            assert_eq!(mine["entries"][0]["name"], "Band");

            let (status, _) = app.api(Method::POST, "/v1/artist/0/tags", Some(json!({ "tags": ["x"] }))).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            sqlx::query("INSERT INTO editing_suspensions (user_id) VALUES ($1)")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let (status, _) = app.api(Method::POST, &path, Some(json!({ "tags": ["more"] }))).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        })
        .await;
    }

    /// A tag shows on an entity once five week-old accounts put it there,
    /// counted as a recount gives; the pages carry an ETag.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn counts_show_past_the_minimum() {
        with_app(CompatConfig::default(), |app| async move {
            let band = band(&app).await;
            let mut sessions = Vec::new();
            for i in 0..6 {
                let (id, _, session) = app.other_user(&format!("fan{i}")).await;
                let days = if i == 5 { 1 } else { 30 };
                sqlx::query(
                    "UPDATE users SET created_at = NOW() - make_interval(days => $2) WHERE id = $1",
                )
                .bind(id)
                .bind(days)
                .execute(&app.pool)
                .await
                .unwrap();
                sessions.push(session);
            }
            let path = format!("/v1/artist/{band}/tags");
            for (i, session) in sessions.iter().enumerate() {
                let tags = if i < 4 {
                    json!(["shoegaze", "dream pop"])
                } else {
                    json!(["shoegaze"])
                };
                let (status, _) = app
                    .api_as(session, Method::POST, &path, Some(json!({ "tags": tags })))
                    .await;
                assert_eq!(status, StatusCode::OK);
            }
            recount(&app).await;

            let (status, headers, body) = app.get(&path).await;
            assert_eq!(status, StatusCode::OK);
            let top: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                top["tags"],
                json!([{ "name": "shoegaze", "taggers": 5 }]),
                "the 1-day-old account weighs nothing, dream pop has 4"
            );
            assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=300");
            let etag = headers[header::ETAG].clone();
            let req = Request::get(&path)
                .header(header::IF_NONE_MATCH, etag.clone())
                .body(Body::empty())
                .unwrap();
            let (status, again, _) = app.send(req).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED);
            assert_eq!(again[header::ETAG], etag);

            let (status, _, body) = app.get("/v1/tag/ShoeGaze/artists").await;
            assert_eq!(status, StatusCode::OK);
            let page: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(page["tag"], "shoegaze");
            assert_eq!(page["total"], 1);
            assert_eq!(page["entries"][0]["id"], band);
            assert_eq!(page["entries"][0]["taggers"], 5);
            assert_eq!(
                app.get("/v1/tag/dream%20pop/albums").await.0,
                StatusCode::OK
            );
            assert_eq!(
                app.get("/v1/tag/unknown/artists").await.0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                app.get("/v1/tag/a%2Fb/artists").await.0,
                StatusCode::BAD_REQUEST
            );

            // An untag is recounted.
            app.api_as(
                &sessions[0],
                Method::DELETE,
                &format!("{path}/shoegaze"),
                None,
            )
            .await;
            recount(&app).await;
            let top: Value = serde_json::from_str(&app.get(&path).await.2).unwrap();
            assert_eq!(top["tags"], json!([]));
        })
        .await;
    }

    /// A user's tags are as visible as the profile.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_users_tags_follow_the_profile() {
        with_app(CompatConfig::default(), |app| async move {
            let band = band(&app).await;
            app.api(
                Method::POST,
                &format!("/v1/artist/{band}/tags"),
                Some(json!({ "tags": ["noise"] })),
            )
            .await;
            let path = format!("/v1/user/{}/tags", app.username);
            let (status, _, body) = app.get(&path).await;
            assert_eq!(status, StatusCode::OK);
            let tags: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(tags["tags"], json!([{ "name": "noise", "count": 1 }]));

            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            assert_eq!(app.get(&path).await.0, StatusCode::FORBIDDEN);
            assert_eq!(app.api(Method::GET, &path, None).await.0, StatusCode::OK);

            sqlx::query("UPDATE users SET is_private = FALSE WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let (_, other, other_session) = app.other_user("other").await;
            app.api(Method::POST, &format!("/v1/user/{other}/block"), None)
                .await;
            assert_eq!(
                app.api_as(&other_session, Method::GET, &path, None).await.0,
                StatusCode::NOT_FOUND
            );

            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            assert_eq!(app.get(&path).await.0, StatusCode::NOT_FOUND);
        })
        .await;
    }
}
