mod common;

use chrono::TimeDelta;
use common::with_db;
use db::queries::blocks::{self as blocks_db, Blocked};
use db::queries::users::{self as users_db, Followed};
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

async fn follows(pool: &PgPool, a: i64, b: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM user_follows
         WHERE (follower_id = $1 AND followee_id = $2) OR (follower_id = $2 AND followee_id = $1)",
    )
    .bind(a)
    .bind(b)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A block ends the follows both ways, keeps both from following while it
/// stands, and changes the follow counts accordingly; lifting it brings
/// nothing back.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_block_ends_follows_both_ways_and_stops_new_ones() {
    with_db(true, |pool| async move {
        let (a, b, c) = (
            user(&pool, "a").await,
            user(&pool, "b").await,
            user(&pool, "c").await,
        );
        for (x, y) in [(a, b), (b, a), (c, a), (b, c)] {
            assert_eq!(
                users_db::follow_user(&pool, x, y, 10).await.unwrap(),
                Followed::Now
            );
        }
        assert_eq!(users_db::follow_counts(&pool, a).await.unwrap(), (2, 1));

        assert_eq!(
            blocks_db::block(&pool, a, b, 10).await.unwrap(),
            Blocked::Now
        );
        assert_eq!(
            blocks_db::block(&pool, a, b, 10).await.unwrap(),
            Blocked::Already
        );
        assert_eq!(follows(&pool, a, b).await, 0);
        assert_eq!(users_db::follow_counts(&pool, a).await.unwrap(), (1, 0));
        assert_eq!(users_db::follow_counts(&pool, b).await.unwrap(), (0, 1));
        assert_eq!(
            users_db::follow_user(&pool, b, a, 10).await.unwrap(),
            Followed::BlockedBy
        );
        assert_eq!(
            users_db::follow_user(&pool, a, b, 10).await.unwrap(),
            Followed::Blocking
        );
        let r = blocks_db::relation(&pool, a, b).await.unwrap();
        assert!(r.blocking && !r.blocked_by && !r.following && !r.follows_you);
        let r = blocks_db::relation(&pool, b, a).await.unwrap();
        assert!(!r.blocking && r.blocked_by);
        // Nobody else is touched.
        assert_eq!(users_db::follow_counts(&pool, c).await.unwrap(), (1, 1));

        assert!(blocks_db::unblock(&pool, a, b).await.unwrap());
        assert!(!blocks_db::unblock(&pool, a, b).await.unwrap());
        assert_eq!(follows(&pool, a, b).await, 0);
        assert_eq!(
            users_db::follow_user(&pool, b, a, 10).await.unwrap(),
            Followed::Now
        );

        // The cap counts the blocker's blocks.
        assert_eq!(
            blocks_db::block(&pool, a, c, 1).await.unwrap(),
            Blocked::Now
        );
        assert_eq!(
            blocks_db::block(&pool, a, b, 1).await.unwrap(),
            Blocked::AtLimit
        );
    })
    .await;
}

/// Follows and blocks between the same two accounts, at the same moment
/// and in any order, never leave a follow beside a block.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn no_follow_survives_a_concurrent_block() {
    with_db(true, |pool| async move {
        for round in 0..20 {
            let a = user(&pool, &format!("a{round}")).await;
            let b = user(&pool, &format!("b{round}")).await;
            let (follow, block, follow_back) = tokio::join!(
                users_db::follow_user(&pool, b, a, 10),
                blocks_db::block(&pool, a, b, 10),
                users_db::follow_user(&pool, a, b, 10),
            );
            follow.unwrap();
            follow_back.unwrap();
            assert_eq!(block.unwrap(), Blocked::Now);
            assert_eq!(follows(&pool, a, b).await, 0, "round {round}");
        }
    })
    .await;
}

/// A removed follower can't follow again until its cooldown is over, and
/// removing someone who doesn't follow records nothing.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_removed_follower_waits_out_its_cooldown() {
    with_db(true, |pool| async move {
        let (a, fan, other) = (
            user(&pool, "a").await,
            user(&pool, "fan").await,
            user(&pool, "other").await,
        );
        users_db::follow_user(&pool, fan, a, 10).await.unwrap();
        assert!(
            blocks_db::remove_follower(&pool, a, fan, TimeDelta::days(30))
                .await
                .unwrap()
        );
        assert!(
            !blocks_db::remove_follower(&pool, a, other, TimeDelta::days(30))
                .await
                .unwrap()
        );
        assert_eq!(users_db::follow_counts(&pool, a).await.unwrap(), (0, 0));
        assert_eq!(
            users_db::follow_user(&pool, fan, a, 10).await.unwrap(),
            Followed::Removed
        );
        // The remover may still follow the removed account.
        assert_eq!(
            users_db::follow_user(&pool, a, fan, 10).await.unwrap(),
            Followed::Now
        );
        let removals: i64 = sqlx::query_scalar("SELECT count(*) FROM follower_removals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(removals, 1);

        assert_eq!(blocks_db::purge_expired_removals(&pool).await.unwrap(), 0);
        sqlx::query("UPDATE follower_removals SET until = NOW()")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            users_db::follow_user(&pool, fan, a, 10).await.unwrap(),
            Followed::Now
        );
        assert_eq!(blocks_db::purge_expired_removals(&pool).await.unwrap(), 1);
    })
    .await;
}
