//! Reports of bio revisions and tags, for an operator's
//! `worker moderation` (`db::queries::moderation`).

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::moderation::{self as moderation_db, Target};
use db::queries::{bios as bios_db, tags as tags_db};
use shared::community;
use shared::models::CatalogKind;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportReason {
    Spam,
    Vandalism,
    Offensive,
    PersonalData,
    Copyright,
    Other,
}

impl ReportReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Vandalism => "vandalism",
            Self::Offensive => "offensive",
            Self::PersonalData => "personal_data",
            Self::Copyright => "copyright",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReportRequest {
    /// A bio revision; or `tag` instead.
    pub revision_id: Option<i64>,
    /// A tag: on the artist, album or track named by one of `artist_id`,
    /// `album_id` and `track_id`, or anywhere without one.
    pub tag: Option<String>,
    pub artist_id: Option<i64>,
    pub album_id: Option<i64>,
    pub track_id: Option<i64>,
    pub reason: ReportReason,
    /// What a moderator should know: one line, 500 characters at most.
    #[serde(default)]
    pub note: String,
}

async fn target(state: &AppState, user_id: i64, req: &ReportRequest) -> ApiResult<Target> {
    let on = match (req.artist_id, req.album_id, req.track_id) {
        (None, None, None) => None,
        (Some(id), None, None) => Some((CatalogKind::Artist, id)),
        (None, Some(id), None) => Some((CatalogKind::Album, id)),
        (None, None, Some(id)) => Some((CatalogKind::Track, id)),
        _ => {
            return Err(AppError::BadRequest(
                "name one of artist_id, album_id and track_id at most".into(),
            ));
        }
    };
    match (req.revision_id, req.tag.as_deref()) {
        (Some(id), None) if on.is_none() => {
            let mut conn = state.db.acquire().await?;
            bios_db::revision(&mut conn, id, Some(user_id))
                .await?
                .ok_or(AppError::NotFound)?;
            Ok(Target::Revision(id))
        }
        (None, Some(tag)) => {
            let tag = community::tag_name(tag).map_err(|e| AppError::BadRequest(e.to_string()))?;
            let min = i64::from(state.rankings.min_listeners) * shared::ranking::FULL;
            let tag_id = tags_db::visible_tag_id(&state.db, &tag, min)
                .await?
                .ok_or(AppError::NotFound)?;
            if let Some((kind, id)) = on {
                super::tags::ensure_entity(state, kind, id).await?;
            }
            Ok(Target::Tag { tag_id, on })
        }
        _ => Err(AppError::BadRequest(
            "report a revision_id, or a tag (maybe on an entity)".into(),
        )),
    }
}

/// POST /v1/reports
pub async fn report(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Json(req): Json<ReportRequest>,
) -> ApiResult<StatusCode> {
    if !limits::report_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    let note = community::line(&req.note, community::NOTE_MAX_CHARS)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let target = target(&state, auth_user.id, &req).await?;
    moderation_db::report(&state.db, auth_user.id, target, req.reason.as_str(), &note).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _report_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Report a bio revision or a tag")
        .description(&format!("Puts a bio revision, or a tag (on one artist, album or track, or anywhere), in the queue the server's operator reviews: they can revert or lock a bio, hide a revision, take a tag off, block it, or suspend an account from editing and tagging. A tag can be reported where it can be seen: shown on an entity, or on a public profile. A second report of the same target while the first is open changes nothing. The reporter isn't notified of what came of it; its export lists its reports, with whether each is open, resolved or dismissed. Nobody is told who reported what. {} reports an hour per account, {} per address.", limits::REPORTS_PER_USER, limits::REPORTS_PER_IP))
        .tag("Bios")
        .response_with::<204, (), _>(|r| r.description("Reported"))
        .response_with::<400, ErrorJson, _>(|r| {
            r.description("Neither or both of a revision and a tag, more than one entity, an invalid tag, or a note too long or with unsafe characters")
        })
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("No such revision (or one held and not yours) or entity, or a tag shown nowhere and on no public profile")
        })
        .response_with::<429, ErrorJson, _>(|r| {
            r.description(&format!(
                "Over {} reports an hour for this account or {} from this address, or over the request limit for this address",
                limits::REPORTS_PER_USER,
                limits::REPORTS_PER_IP
            ))
        })
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use crate::compat::CompatConfig;
    use crate::test_app::with_app;

    /// Reports of revisions and tags land in the queue once per target.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn reports_queue_once_per_target() {
        with_app(CompatConfig::default(), |app| async move {
            sqlx::query("UPDATE users SET created_at = NOW() - INTERVAL '30 days' WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            let band = db::queries::tracks::find_or_create_artist(&app.pool, "Band")
                .await
                .unwrap()
                .id;
            let (_, revision) = app
                .api(
                    Method::PUT,
                    &format!("/v1/artist/{band}/bio"),
                    Some(json!({ "body": "Text.", "base_revision_id": null })),
                )
                .await;
            app.api(
                Method::POST,
                &format!("/v1/artist/{band}/tags"),
                Some(json!({ "tags": ["noise"] })),
            )
            .await;
            let (_, _, reporter) = app.other_user("reporter").await;
            // A tag only a private account uses can't be reported: nobody sees it.
            let (hider, _, hider_session) = app.other_user("hider").await;
            app.api_as(
                &hider_session,
                Method::POST,
                &format!("/v1/artist/{band}/tags"),
                Some(json!({ "tags": ["secret"] })),
            )
            .await;
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(hider)
                .execute(&app.pool)
                .await
                .unwrap();
            for body in [
                json!({ "revision_id": revision["id"], "reason": "vandalism" }),
                json!({ "revision_id": revision["id"], "reason": "spam", "note": "again" }),
                json!({ "tag": "NOISE", "artist_id": band, "reason": "offensive" }),
                json!({ "tag": "noise", "reason": "spam" }),
            ] {
                let (status, _) = app
                    .api_as(&reporter, Method::POST, "/v1/reports", Some(body.clone()))
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            }
            let open: i64 =
                sqlx::query_scalar("SELECT count(*) FROM reports WHERE status = 'open'")
                    .fetch_one(&app.pool)
                    .await
                    .unwrap();
            assert_eq!(open, 3, "the second report of the revision changed nothing");

            for (body, expected) in [
                (json!({ "reason": "spam" }), StatusCode::BAD_REQUEST),
                (
                    json!({ "revision_id": revision["id"], "tag": "noise", "reason": "spam" }),
                    StatusCode::BAD_REQUEST,
                ),
                (
                    json!({ "tag": "noise", "artist_id": band, "track_id": 1, "reason": "spam" }),
                    StatusCode::BAD_REQUEST,
                ),
                (
                    json!({ "revision_id": 0, "reason": "spam" }),
                    StatusCode::NOT_FOUND,
                ),
                (
                    json!({ "tag": "unused", "reason": "spam" }),
                    StatusCode::NOT_FOUND,
                ),
                (
                    json!({ "tag": "secret", "reason": "spam" }),
                    StatusCode::NOT_FOUND,
                ),
                (
                    json!({ "tag": "noise", "album_id": 0, "reason": "spam" }),
                    StatusCode::NOT_FOUND,
                ),
                (
                    json!({ "tag": "noise", "reason": "spam", "note": "x".repeat(501) }),
                    StatusCode::BAD_REQUEST,
                ),
            ] {
                let (status, _) = app
                    .api_as(&reporter, Method::POST, "/v1/reports", Some(body.clone()))
                    .await;
                assert_eq!(status, expected, "{body}");
            }
        })
        .await;
    }
}
