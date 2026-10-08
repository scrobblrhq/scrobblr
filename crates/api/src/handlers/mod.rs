pub mod auth;
pub mod community;
pub mod connected_accounts;
pub mod imports;
pub mod scrobbles;
pub mod tracks;
pub mod uploads;
pub mod users;

use schemars::JsonSchema;
use serde::Deserialize;

/// Path parameters, named so the OpenAPI spec documents them.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdPath {
    pub id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UuidPath {
    pub id: uuid::Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UsernamePath {
    pub username: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProviderPath {
    /// `spotify` or `lastfm`.
    pub provider: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TokenPath {
    pub token: String,
}

/// A page size from the query: `default` when absent, within 1 to `max`.
pub fn page_limit(requested: Option<i64>, default: i64, max: i64) -> i64 {
    requested.unwrap_or(default).clamp(1, max)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::Utc;
    use serde_json::json;

    use crate::test_app::with_app;

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn out_of_range_input_is_refused_or_bounded() {
        with_app(Default::default(), |app| async move {
            let recent = format!("/v1/user/{}/recent?limit=-5", app.username);
            assert_eq!(app.api(Method::GET, &recent, None).await.0, StatusCode::OK);
            let search = format!("/v1/search?q={}", "a".repeat(201));
            let (status, _) = app.api(Method::GET, &search, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let rename = |name: &str| json!({ "display_name": name });
            for name in ["x".repeat(41), "evil\u{202E}name".into()] {
                let (status, _) = app
                    .api(Method::PATCH, "/v1/user/me", Some(rename(&name)))
                    .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
            }
            let (status, profile) = app
                .api(Method::PATCH, "/v1/user/me", Some(rename("  Chewawi ")))
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(profile["display_name"], "Chewawi");

            let (status, _) = app
                .api(Method::POST, "/v1/auth/tokens", Some(json!({ "name": " " })))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let play = json!({
                "track": "Song",
                "artist": "Someone",
                "played_at": Utc::now().to_rfc3339(),
                "duration_ms": -1,
                "source": "s".repeat(500),
            });
            let (status, _) = app.api(Method::POST, "/v1/scrobble", Some(play)).await;
            assert_eq!(status, StatusCode::CREATED);
            let (duration, source): (Option<i32>, String) = sqlx::query_as(
                "SELECT t.duration_ms, s.source FROM scrobbles s JOIN tracks t ON t.id = s.track_id",
            )
            .fetch_one(&app.pool)
            .await
            .unwrap();
            assert_eq!(duration, None);
            assert_eq!(source.len(), 100);
        })
        .await;
    }
}
