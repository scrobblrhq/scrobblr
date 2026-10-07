mod common;

use chrono::TimeDelta;
use common::with_db;
use db::queries::scrobblers::{self as scrobblers_db, KIND_SESSION, KIND_TOKEN, NewCredential};
use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', 'x') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn credential<'a>(user_id: i64, kind: &'a str, hash: &'a str) -> NewCredential<'a> {
    NewCredential {
        user_id,
        kind,
        name: "Pano Scrobbler",
        key_hash: hash,
        legacy_secret: None,
        api_key: None,
    }
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn credentials_resolve_to_their_owner_and_revoke_per_user() {
    with_db(true, |pool| async move {
        let ana = user(&pool, "Ana").await;
        let bo = user(&pool, "bo").await;
        let token = scrobblers_db::create_credential(
            &pool,
            &NewCredential {
                legacy_secret: Some("v1:ciphertext"),
                ..credential(ana, KIND_TOKEN, "hash-token")
            },
        )
        .await
        .unwrap();
        assert!(token.legacy_auth);
        let session = scrobblers_db::create_credential(
            &pool,
            &NewCredential {
                api_key: Some("panoScrobbler"),
                ..credential(ana, KIND_SESSION, "hash-session")
            },
        )
        .await
        .unwrap();
        assert!(!session.legacy_auth);

        let found = scrobblers_db::find_credential(&pool, "hash-session")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((found.user_id, found.username.as_str()), (ana, "Ana"));
        assert_eq!(found.api_key.as_deref(), Some("panoScrobbler"));
        assert!(
            scrobblers_db::find_credential(&pool, "nope")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            scrobblers_db::list_credentials(&pool, ana)
                .await
                .unwrap()
                .len(),
            2
        );

        // Only token-kind credentials with a legacy secret, matched like logins.
        let candidates = scrobblers_db::legacy_candidates(&pool, "ANA")
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].credential.id, token.id);
        assert!(
            scrobblers_db::legacy_candidates(&pool, "nobody")
                .await
                .unwrap()
                .is_empty()
        );

        assert!(
            !scrobblers_db::delete_credential(&pool, token.id, bo)
                .await
                .unwrap()
        );
        assert!(
            scrobblers_db::delete_credential(&pool, token.id, ana)
                .await
                .unwrap()
        );
        assert!(
            scrobblers_db::find_credential(&pool, "hash-token")
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn an_authorization_is_approved_once_and_traded_once() {
    with_db(true, |pool| async move {
        let ana = user(&pool, "ana").await;
        let bo = user(&pool, "bo").await;
        let hour = TimeDelta::hours(1);
        scrobblers_db::create_authorization(&pool, "tok", "key", None, hour)
            .await
            .unwrap();
        // Not approved yet: nothing to trade.
        assert_eq!(
            scrobblers_db::take_authorization(&pool, "tok", "key")
                .await
                .unwrap(),
            None
        );
        assert!(
            scrobblers_db::approve_authorization(&pool, "tok", ana)
                .await
                .unwrap()
        );
        assert!(
            !scrobblers_db::approve_authorization(&pool, "tok", bo)
                .await
                .unwrap()
        );
        assert_eq!(
            scrobblers_db::take_authorization(&pool, "tok", "other")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            scrobblers_db::take_authorization(&pool, "tok", "key")
                .await
                .unwrap(),
            Some(ana)
        );
        assert_eq!(
            scrobblers_db::take_authorization(&pool, "tok", "key")
                .await
                .unwrap(),
            None
        );

        scrobblers_db::create_authorization(&pool, "old", "key", Some(ana), -hour)
            .await
            .unwrap();
        assert_eq!(
            scrobblers_db::take_authorization(&pool, "old", "key")
                .await
                .unwrap(),
            None
        );
        assert!(
            !scrobblers_db::approve_authorization(&pool, "old", bo)
                .await
                .unwrap()
        );
        assert_eq!(
            scrobblers_db::delete_expired_authorizations(&pool)
                .await
                .unwrap(),
            1
        );
        assert!(
            scrobblers_db::get_authorization(&pool, "old")
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
}
