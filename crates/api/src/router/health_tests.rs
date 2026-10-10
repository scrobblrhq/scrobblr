use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::monitoring::Monitoring;
use db::queries::monitoring as mdb;

const TOKEN: &str = "0123456789abcdef-metrics";

/// A router whose database and Redis never answer.
fn offline_app(metrics_token: Option<&str>) -> Router {
    let mut state = super::openapi_tests::offline_state();
    // Its INCR would wait for a Redis that never comes.
    state.rate_limit.requests = 0;
    state.monitoring = Arc::new(Monitoring::new(3.0, metrics_token.map(Into::into)));
    super::build(state)
}

async fn get(app: &Router, path: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::get(path);
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn health_is_unavailable_while_the_database_and_redis_are_away() {
    let app = offline_app(None);
    assert_eq!(
        get(&app, "/health", None).await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable: database, redis".into()
        )
    );
    assert_eq!(
        get(&app, "/health/worker", None).await,
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable".into())
    );
}

#[tokio::test]
async fn metrics_take_their_token() {
    assert_eq!(
        get(&offline_app(None), "/metrics", Some(TOKEN)).await.0,
        StatusCode::NOT_FOUND
    );
    let app = offline_app(Some(TOKEN));
    assert_eq!(
        get(&app, "/metrics", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&app, "/metrics", Some("0123456789abcdef-wrong"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = get(&app, "/metrics", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("scrobblr_up{dependency=\"database\"} 0\n"),
        "{body}"
    );
    // The two refusals before.
    assert!(
        body.contains("scrobblr_http_responses_total{class=\"4xx\"} 2\n"),
        "{body}"
    );
    assert!(
        body.contains("scrobblr_http_rate_limited_total 0\n"),
        "{body}"
    );
    assert!(!body.contains("scrobblr_worker_healthy"), "{body}");
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn health_reports_the_worker_and_metrics_its_loops() {
    crate::test_app::with_custom_app(
        |state| state.monitoring = Arc::new(Monitoring::new(3.0, Some(TOKEN.into()))),
        |app| async move {
            assert_eq!(app.get("/health").await.0, StatusCode::OK);
            // No heartbeats: the worker never ran.
            assert_eq!(
                app.get("/health/worker").await.0,
                StatusCode::SERVICE_UNAVAILABLE
            );

            mdb::register_loops(&app.pool, &[("classification", 60), ("rankings", 60)])
                .await
                .unwrap();
            mdb::record_run(&app.pool, "classification", 60, None)
                .await
                .unwrap();
            mdb::record_run(&app.pool, "rankings", 60, None)
                .await
                .unwrap();
            assert_eq!(app.get("/health/worker").await.0, StatusCode::OK);

            sqlx::query(
                "UPDATE worker_heartbeats SET last_run_at = NOW() - interval '1 hour', \
                 started_at = NOW() - interval '2 hours' WHERE loop_name = 'rankings'",
            )
            .execute(&app.pool)
            .await
            .unwrap();
            let (status, _, body) = app.get("/health/worker").await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::SERVICE_UNAVAILABLE, "unhealthy")
            );

            let req = Request::get("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap();
            let (status, _, body) = app.send(req).await;
            assert_eq!(status, StatusCode::OK);
            for line in [
                "scrobblr_up{dependency=\"database\"} 1",
                "scrobblr_up{dependency=\"redis\"} 1",
                "scrobblr_worker_healthy 0",
                "scrobblr_worker_loop_healthy{loop=\"classification\"} 1",
                "scrobblr_worker_loop_healthy{loop=\"rankings\"} 0",
                "scrobblr_worker_loop_deadline_seconds{loop=\"rankings\"} 180",
                "scrobblr_queue_due{queue=\"classification\"} 0",
                "scrobblr_http_responses_total{class=\"2xx\"} 2",
                "scrobblr_http_responses_total{class=\"5xx\"} 2",
            ] {
                assert!(body.lines().any(|l| l == line), "no `{line}` in\n{body}");
            }
        },
    )
    .await;
}
