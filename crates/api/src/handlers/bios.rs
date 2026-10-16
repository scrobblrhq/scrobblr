//! Community bios of artists and albums (`db::queries::bios`;
//! docs/social.md, Bios).

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;

use super::IdPath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::bios::{self as bios_db, Change, Written};
use db::queries::tracks as tracks_db;
use shared::community::{self, Refusal};
use shared::models::{Bio, BioHistoryPage, BioRevision, CatalogKind, RevisionStatus};

async fn ensure_entity(state: &AppState, kind: CatalogKind, id: i64) -> ApiResult<()> {
    let exists = match kind {
        CatalogKind::Artist => tracks_db::find_artist_by_id(&state.db, id).await?.is_some(),
        CatalogKind::Album => tracks_db::find_album_by_id(&state.db, id).await?.is_some(),
        CatalogKind::Track => false,
    };
    exists.then_some(()).ok_or(AppError::NotFound)
}

fn viewer(auth_user: Option<Extension<AuthUser>>) -> Option<i64> {
    auth_user.map(|Extension(a)| a.id)
}

fn name(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Artist => "Artist",
        CatalogKind::Album => "Album",
        CatalogKind::Track => "Track",
    }
}

const AUTHORS: &str = "An author is named unless the account is deleted or waiting to be, or blocks the signed-in viewer or is blocked by it; imported text and moderators' reverts have none.";

async fn current(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    viewer: Option<i64>,
) -> ApiResult<Json<Bio>> {
    ensure_entity(state, kind, id).await?;
    Ok(Json(bios_db::current(&state.db, kind, id, viewer).await?))
}

/// GET /v1/artist/:id/bio
pub async fn artist_bio(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<Bio>> {
    current(&state, CatalogKind::Artist, id, viewer(auth_user)).await
}

/// GET /v1/album/:id/bio
pub async fn album_bio(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<Bio>> {
    current(&state, CatalogKind::Album, id, viewer(auth_user)).await
}

fn current_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    op.summary(&format!("{} bio", name(kind)))
        .description(&format!("The bio as it stands: its current revision, written by the community or imported from Last.fm before anyone did. `revision_id` and `body` are `null` when there is none. The text is plain: show it as text, a paragraph at each blank line, `http://` and `https://` URLs as links marked `rel=\"nofollow ugc\"`, never embedded. {AUTHORS}"))
        .tag("Bios")
        .response::<200, Json<Bio>>()
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("{} not found", name(kind))))
}

pub fn _artist_bio_doc(op: TransformOperation) -> TransformOperation {
    current_doc(op, CatalogKind::Artist)
}

pub fn _album_bio_doc(op: TransformOperation) -> TransformOperation {
    current_doc(op, CatalogKind::Album)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryQuery {
    /// Revisions per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// The previous page's `next`, for the page after it.
    pub before: Option<String>,
}

async fn history(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    q: &HistoryQuery,
    viewer: Option<i64>,
) -> ApiResult<Json<BioHistoryPage>> {
    ensure_entity(state, kind, id).await?;
    let before = q
        .before
        .as_deref()
        .map(|b| {
            b.parse::<i64>()
                .map_err(|_| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let limit = super::page_limit(q.limit, 50, 200);
    let mut entries = bios_db::history(&state.db, kind, id, before, limit + 1, viewer).await?;
    let next = if entries.len() as i64 > limit {
        entries.truncate(limit as usize);
        entries.last().map(|r| r.id.to_string())
    } else {
        None
    };
    Ok(Json(BioHistoryPage { entries, next }))
}

/// GET /v1/artist/:id/bio/history
pub async fn artist_bio_history(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<HistoryQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<BioHistoryPage>> {
    history(&state, CatalogKind::Artist, id, &q, viewer(auth_user)).await
}

/// GET /v1/album/:id/bio/history
pub async fn album_bio_history(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<HistoryQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<BioHistoryPage>> {
    history(&state, CatalogKind::Album, id, &q, viewer(auth_user)).await
}

fn history_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    op.summary(&format!("{} bio history", name(kind)))
        .description(&format!("Every published revision of the bio, newest first, `limit` a page (50 by default, 200 at most): pass `next` as `before` for the following page; `next` is `null` on the last. Entries carry no `body`: `GET /v1/bio/revisions/{{id}}` has it. A revision a moderator hid stays, with `hidden: true` and neither summary nor text. Edits held for review aren't listed. {AUTHORS}"))
        .tag("Bios")
        .response::<200, Json<BioHistoryPage>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("{} not found", name(kind))))
}

pub fn _artist_bio_history_doc(op: TransformOperation) -> TransformOperation {
    history_doc(op, CatalogKind::Artist)
}

pub fn _album_bio_history_doc(op: TransformOperation) -> TransformOperation {
    history_doc(op, CatalogKind::Album)
}

/// GET /v1/bio/revisions/:id
pub async fn get_revision(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<BioRevision>> {
    let mut conn = state.db.acquire().await?;
    bios_db::revision(&mut conn, id, viewer(auth_user))
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

pub fn _get_revision_doc(op: TransformOperation) -> TransformOperation {
    op.summary("A bio revision")
        .description(&format!("One revision of a bio, with its text (`null` when a moderator hid it). An edit held for review, or one a moderator rejected, is its author's to see only. {AUTHORS}"))
        .tag("Bios")
        .response::<200, Json<BioRevision>>()
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("No such revision, or one held or rejected and not yours")
        })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditBioRequest {
    /// The new text: plain text, at most 10,000 characters and five links,
    /// no control, invisible or bidirectional formatting characters.
    pub body: String,
    /// What changed, for the history: one line, 200 characters at most.
    #[serde(default)]
    pub summary: String,
    /// `revision_id` of the bio the text was written over; `null` when it
    /// had none. Another revision since is a 409.
    pub base_revision_id: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevertBioRequest {
    /// The earlier revision whose text to restore.
    pub revision_id: i64,
    /// As in an edit: the current revision as read.
    pub base_revision_id: Option<i64>,
    #[serde(default)]
    pub summary: String,
}

/// Whether the account may write, and within its hourly limits.
async fn may_write(state: &AppState, user_id: i64, ip: &str) -> ApiResult<()> {
    if !limits::bio_attempt(state, user_id, ip).await {
        return Err(AppError::RateLimited);
    }
    let editor = bios_db::editor(&state.db, user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let min_days = state.rankings.min_account_days;
    match editor.refusal(Utc::now(), min_days) {
        None => Ok(()),
        Some(Refusal::TooNew(from)) => Err(AppError::NotAllowed(format!(
            "accounts may edit bios from {min_days} days after registering: yours from {}",
            from.format("%Y-%m-%d %H:%M UTC")
        ))),
        Some(Refusal::NotNow) => Err(AppError::NotAllowed(
            "this account can't edit bios now".into(),
        )),
    }
}

fn written(result: Written) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    match result {
        Written::Revision(r) => {
            let status = match r.status {
                RevisionStatus::Held => StatusCode::ACCEPTED,
                _ => StatusCode::OK,
            };
            Ok((status, Json(*r)))
        }
        Written::Conflict { current } => Err(AppError::Conflict(match current {
            Some(id) => format!("the bio changed since: its current revision is {id}"),
            None => "the bio changed since: it has no text now".into(),
        })),
        Written::Locked => Err(AppError::NotAllowed("a moderator locked this bio".into())),
        Written::Unchanged => Err(AppError::BadRequest(
            "that is the bio's current text".into(),
        )),
        Written::DailyCap => Err(AppError::Limited(format!(
            "at most {} bio revisions in 24 hours",
            community::EDITS_PER_DAY
        ))),
        Written::BioCap => Err(AppError::Limited(format!(
            "at most {} revisions of one bio in 24 hours",
            community::EDITS_PER_BIO_PER_DAY
        ))),
        Written::BadTarget => Err(AppError::BadRequest(
            "revision_id isn't a published, visible revision of this bio".into(),
        )),
    }
}

fn text_error(e: community::TextError) -> AppError {
    AppError::BadRequest(e.to_string())
}

async fn edit(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    user_id: i64,
    ip: &str,
    req: EditBioRequest,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    ensure_entity(state, kind, id).await?;
    may_write(state, user_id, ip).await?;
    let body = community::bio_text(&req.body).map_err(text_error)?;
    let summary =
        community::line(&req.summary, community::SUMMARY_MAX_CHARS).map_err(text_error)?;
    let change = Change {
        kind,
        entity_id: id,
        author_id: user_id,
        base: req.base_revision_id,
        summary: &summary,
    };
    written(bios_db::edit(&state.db, &change, &body).await?)
}

/// PUT /v1/artist/:id/bio
pub async fn edit_artist_bio(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<EditBioRequest>,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    edit(&state, CatalogKind::Artist, id, auth_user.id, &ip, req).await
}

/// PUT /v1/album/:id/bio
pub async fn edit_album_bio(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<EditBioRequest>,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    edit(&state, CatalogKind::Album, id, auth_user.id, &ip, req).await
}

fn write_limits() -> String {
    format!(
        "Over {} edits and reverts an hour for this account or {} from this address, {} revisions in 24 hours, or {} of one bio; or over the request limit for this address",
        limits::BIO_EDITS_PER_USER,
        limits::BIO_EDITS_PER_IP,
        community::EDITS_PER_DAY,
        community::EDITS_PER_BIO_PER_DAY
    )
}

const MAY_EDIT: &str = "Accounts may edit some days after registering (a 403 says from when), unless they can't edit for now (a 403 saying so); nobody may edit a bio a moderator locked (403). An account writes 50 revisions in 24 hours at most, and 3 of one bio, so two accounts can't keep reverting each other.";

fn edit_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    op.summary(&format!("Edit an {} bio", name(kind).to_lowercase()))
        .description(&format!("Writes a new revision of the bio over `base_revision_id`, the revision the text was written over; if another came since, nothing is written (409: read the bio again and merge). {MAY_EDIT} An edit that looks like spam is held for a moderator: 202, with `status: held` and why in `held_reason`, and the bio unchanged."))
        .tag("Bios")
        .response::<200, Json<BioRevision>>()
        .response_with::<202, Json<BioRevision>, _>(|r| r.description("Held for review: the bio is unchanged"))
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("Text empty, too long, with too many links or unsafe characters, or the current text already")
        })
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("{} not found", name(kind))))
        .response_with::<409, ErrorJson, _>(|r| r.description("The bio changed since `base_revision_id`"))
        .response_with::<429, ErrorJson, _>(|r| r.description(&write_limits()))
}

pub fn _edit_artist_bio_doc(op: TransformOperation) -> TransformOperation {
    edit_doc(op, CatalogKind::Artist)
}

pub fn _edit_album_bio_doc(op: TransformOperation) -> TransformOperation {
    edit_doc(op, CatalogKind::Album)
}

async fn revert(
    state: &AppState,
    kind: CatalogKind,
    id: i64,
    user_id: i64,
    ip: &str,
    req: RevertBioRequest,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    ensure_entity(state, kind, id).await?;
    may_write(state, user_id, ip).await?;
    let summary =
        community::line(&req.summary, community::SUMMARY_MAX_CHARS).map_err(text_error)?;
    let change = Change {
        kind,
        entity_id: id,
        author_id: user_id,
        base: req.base_revision_id,
        summary: &summary,
    };
    written(bios_db::revert(&state.db, &change, req.revision_id).await?)
}

/// POST /v1/artist/:id/bio/revert
pub async fn revert_artist_bio(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<RevertBioRequest>,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    revert(&state, CatalogKind::Artist, id, auth_user.id, &ip, req).await
}

/// POST /v1/album/:id/bio/revert
pub async fn revert_album_bio(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
    Json(req): Json<RevertBioRequest>,
) -> ApiResult<(StatusCode, Json<BioRevision>)> {
    revert(&state, CatalogKind::Album, id, auth_user.id, &ip, req).await
}

fn revert_doc(op: TransformOperation, kind: CatalogKind) -> TransformOperation {
    op.summary(&format!("Revert an {} bio", name(kind).to_lowercase()))
        .description(&format!("Restores the text of `revision_id`, an earlier published revision of this bio that no moderator hid, as a new revision (`kind: revert`) over `base_revision_id`, as an edit would; a revert is never held for review. {MAY_EDIT} Reverts count with edits."))
        .tag("Bios")
        .response::<200, Json<BioRevision>>()
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("`revision_id` isn't a published, visible revision of this bio, or its text is the current one")
        })
        .response_with::<404, ErrorJson, _>(|r| r.description(&format!("{} not found", name(kind))))
        .response_with::<409, ErrorJson, _>(|r| r.description("The bio changed since `base_revision_id`"))
        .response_with::<429, ErrorJson, _>(|r| r.description(&write_limits()))
}

pub fn _revert_artist_bio_doc(op: TransformOperation) -> TransformOperation {
    revert_doc(op, CatalogKind::Artist)
}

pub fn _revert_album_bio_doc(op: TransformOperation) -> TransformOperation {
    revert_doc(op, CatalogKind::Album)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::compat::CompatConfig;
    use crate::test_app::{TestApp, with_app};

    async fn aged(app: &TestApp, user_id: i64) {
        sqlx::query("UPDATE users SET created_at = NOW() - INTERVAL '30 days' WHERE id = $1")
            .bind(user_id)
            .execute(&app.pool)
            .await
            .unwrap();
    }

    async fn band(app: &TestApp) -> i64 {
        db::queries::tracks::find_or_create_artist(&app.pool, "Band")
            .await
            .unwrap()
            .id
    }

    async fn put(app: &TestApp, bearer: &str, artist: i64, body: Value) -> (StatusCode, Value) {
        app.api_as(
            bearer,
            Method::PUT,
            &format!("/v1/artist/{artist}/bio"),
            Some(body),
        )
        .await
    }

    /// Who may edit: a week-old account; not a new one, a suspended one,
    /// one the coordination detection flagged, nor a token without `write`;
    /// nobody once a moderator locks the bio.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn only_eligible_accounts_edit() {
        with_app(CompatConfig::default(), |app| async move {
            let band = band(&app).await;
            let me = app.session.to_string();
            let text = json!({ "body": "Formed in 1990.", "base_revision_id": null });

            let (status, body) = put(&app, &me, band, text.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert!(body["error"].as_str().unwrap().contains("7 days"), "{body}");

            aged(&app, app.user_id).await;
            let (status, first) = put(&app, &me, band, text.clone()).await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_eq!(first["status"], "published");
            assert_eq!(first["author"]["username"], app.username.as_str());

            let (_, flagged, flagged_session) = app.other_user("flagged").await;
            let flagged_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
                .bind(&flagged)
                .fetch_one(&app.pool)
                .await
                .unwrap();
            aged(&app, flagged_id).await;
            sqlx::query(
                "INSERT INTO coordination_days (user_id, day, factor, run_id)
                 VALUES ($1, CURRENT_DATE - 2, 200, 1)",
            )
            .bind(flagged_id)
            .execute(&app.pool)
            .await
            .unwrap();
            let over = json!({ "body": "Spam.", "base_revision_id": first["id"] });
            let (status, body) = put(&app, &flagged_session, band, over.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"], "this account can't edit bios now");

            let (suspended_id, _, suspended_session) = app.other_user("suspended").await;
            aged(&app, suspended_id).await;
            sqlx::query("INSERT INTO editing_suspensions (user_id) VALUES ($1)")
                .bind(suspended_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let (status, body) = put(&app, &suspended_session, band, over.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"], "this account can't edit bios now");

            let (_, token) = app
                .api(
                    Method::POST,
                    "/v1/auth/tokens",
                    Some(json!({ "name": "reader", "scopes": ["read"] })),
                )
                .await;
            let (status, _) = put(&app, token["token"].as_str().unwrap(), band, over.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN);

            sqlx::query("UPDATE entity_bios SET locked_at = NOW() WHERE entity_id = $1")
                .bind(band)
                .execute(&app.pool)
                .await
                .unwrap();
            let mine = json!({ "body": "Mine.", "base_revision_id": first["id"] });
            let (status, body) = put(&app, &me, band, mine).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"], "a moderator locked this bio");
            let (_, bio) = app
                .api(Method::GET, &format!("/v1/artist/{band}/bio"), None)
                .await;
            assert_eq!(bio["locked"], true);

            let (status, _) = put(&app, &me, band + 1000, text).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// Two editors over the same revision: the second is told it changed;
    /// a revert restores the first text; the history pages through; a
    /// held edit is 202 and its author's alone.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn concurrent_edits_conflict_and_reverts_restore() {
        with_app(CompatConfig::default(), |app| async move {
            aged(&app, app.user_id).await;
            let band = band(&app).await;
            let me = app.session.to_string();
            let (other_id, _, other) = app.other_user("other").await;
            aged(&app, other_id).await;

            let (_, first) = put(
                &app,
                &me,
                band,
                json!({ "body": "One.", "base_revision_id": null }),
            )
            .await;
            let base = first["id"].clone();
            let (status, second) = put(
                &app,
                &other,
                band,
                json!({ "body": "Two.", "summary": "fix", "base_revision_id": base }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let (status, body) = put(
                &app,
                &me,
                band,
                json!({ "body": "Three.", "base_revision_id": base }),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains(&second["id"].to_string()),
                "{body}"
            );
            let (status, _) = put(
                &app,
                &me,
                band,
                json!({ "body": "  ", "base_revision_id": second["id"] }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _) = put(
                &app,
                &me,
                band,
                json!({ "body": "x\u{202E}", "base_revision_id": second["id"] }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let revert = format!("/v1/artist/{band}/bio/revert");
            let (status, back) = app
                .api(
                    Method::POST,
                    &revert,
                    Some(json!({ "revision_id": base, "base_revision_id": second["id"] })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{back}");
            assert_eq!(
                (back["kind"].as_str(), back["body"].as_str()),
                (Some("revert"), Some("One."))
            );
            let (_, bio) = app
                .api(Method::GET, &format!("/v1/artist/{band}/bio"), None)
                .await;
            assert_eq!(
                (bio["body"].as_str(), bio["revisions"].as_i64()),
                (Some("One."), Some(3))
            );
            let (_, artist) = app
                .api(Method::GET, &format!("/v1/artist/{band}"), None)
                .await;
            assert_eq!(artist["bio"], "One.");

            let history = format!("/v1/artist/{band}/bio/history?limit=2");
            let (_, page) = app.api(Method::GET, &history, None).await;
            assert_eq!(page["entries"].as_array().unwrap().len(), 2);
            assert_eq!(page["entries"][1]["summary"], "fix");
            let next = page["next"].as_str().unwrap();
            let (_, rest) = app
                .api(Method::GET, &format!("{history}&before={next}"), None)
                .await;
            assert_eq!(rest["entries"][0]["id"], base);
            assert_eq!(rest["next"], Value::Null);
            let (status, _) = app
                .api(Method::GET, &format!("{history}&before=x"), None)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let spam = "https://x.example https://x.example https://x.example";
            let (status, held) = put(
                &app,
                &other,
                band,
                json!({ "body": spam, "base_revision_id": back["id"] }),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert_eq!(held["held_reason"], "repeated_link");
            let revision = format!("/v1/bio/revisions/{}", held["id"]);
            assert_eq!(
                app.api_as(&other, Method::GET, &revision, None).await.0,
                StatusCode::OK
            );
            assert_eq!(
                app.api(Method::GET, &revision, None).await.0,
                StatusCode::NOT_FOUND
            );
        })
        .await;
    }

    /// Authors are named whatever their privacy (an edit is published for
    /// everyone), but not while waiting to be deleted, nor to an account
    /// they block or that blocks them.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn authors_follow_blocks_and_deletion() {
        with_app(CompatConfig::default(), |app| async move {
            aged(&app, app.user_id).await;
            let band = band(&app).await;
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let (_, edit) = put(
                &app,
                &app.session.to_string(),
                band,
                json!({ "body": "One.", "base_revision_id": null }),
            )
            .await;
            let bio = format!("/v1/artist/{band}/bio");
            let revision = format!("/v1/bio/revisions/{}", edit["id"]);
            let author = |v: &Value| v["author"]["username"].as_str().map(str::to_string);

            let signed_out: Value = app.get(&bio).await.2.parse().unwrap();
            assert_eq!(author(&signed_out).as_deref(), Some(app.username.as_str()));

            let (_, blocked, blocked_session) = app.other_user("blocked").await;
            app.api(Method::POST, &format!("/v1/user/{blocked}/block"), None)
                .await;
            let (_, seen) = app.api_as(&blocked_session, Method::GET, &bio, None).await;
            assert_eq!(author(&seen), None);
            assert_eq!(seen["body"], "One.", "the text is the catalog's");
            let (_, seen) = app
                .api_as(&blocked_session, Method::GET, &revision, None)
                .await;
            assert_eq!(author(&seen), None);

            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let (_, history) = app
                .api_as(
                    &blocked_session,
                    Method::GET,
                    &format!("{bio}/history"),
                    None,
                )
                .await;
            assert_eq!(author(&history["entries"][0]), None);
            let signed_out: Value = app.get(&bio).await.2.parse().unwrap();
            assert_eq!(author(&signed_out), None);
        })
        .await;
    }
}
