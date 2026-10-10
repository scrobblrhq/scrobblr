use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn health_is_unavailable_while_the_database_and_redis_are_away() {
    let mut state = super::openapi_tests::offline_state();
    // Its INCR would wait for a Redis that never comes.
    state.rate_limit.requests = 0;
    let response = super::build(state)
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"unavailable: database, redis");
}
