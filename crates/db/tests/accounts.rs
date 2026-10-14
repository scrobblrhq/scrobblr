mod common;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use common::with_db;
use db::queries::accounts as accounts_db;
use db::queries::classification as cdb;
use db::queries::rankings::{self as rdb, Period};
use db::queries::{scrobbles as scrobbles_db, tracks as tracks_db};
use shared::classification::BudgetParams;
use shared::media::{UploadKey, UploadKind};
use shared::ranking::RankingParams;
use sqlx::PgPool;

fn days_ago(n: i64) -> NaiveDate {
    (Utc::now() - TimeDelta::days(n)).date_naive()
}

fn at(day: NaiveDate, secs: i64) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs)
}

async fn exec(pool: &PgPool, sql: &str, id: i64) {
    sqlx::query(sql).bind(id).execute(pool).await.unwrap();
}

/// A user with a row in every table that can hold one: listening history
/// (live, imported, on an old compressed chunk), its labels, weights and
/// coordination factor, credentials of every kind, a linked account, an
/// import, follows, a comment, uploads and likes, now playing, its
/// recommendations and a skip.
async fn populate(pool: &PgPool, name: &str, artwork_artist: i64) -> i64 {
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, created_at, image_url)
         VALUES ($1, $1 || '@example.test', 'x', NOW() - INTERVAL '400 days', $2) RETURNING id",
    )
    .bind(name)
    .bind(UploadKey::generate(UploadKind::Avatar).as_str())
    .fetch_one(pool)
    .await
    .unwrap();

    let artist = tracks_db::find_or_create_artist(pool, "Shared Band")
        .await
        .unwrap();
    let album = tracks_db::find_or_create_album(pool, artist.id, "Shared Album")
        .await
        .unwrap();
    for (day, n) in [(days_ago(3), 6), (days_ago(4), 3), (days_ago(90), 4)] {
        for i in 0..n {
            let title = format!("Song {i}");
            let track = tracks_db::find_or_create_track(
                pool,
                artist.id,
                Some(album),
                &title,
                Some(200_000),
            )
            .await
            .unwrap();
            // Live: the row trigger counts it.
            sqlx::query(
                "INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source,
                                        duration_ms, listened_ms)
                 VALUES ($1, $2, $3, $4, $5, 'test', 200000, 200000)",
            )
            .bind(user_id)
            .bind(track.id)
            .bind(artist.id)
            .bind(album)
            .bind(at(day, 3600 + i * 210))
            .execute(pool)
            .await
            .unwrap();
        }
    }
    // An import, its row counted the way the importer counts it.
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO scrobble_imports (user_id, provider, external_user, verified, status)
         VALUES ($1, 'lastfm', $2, TRUE, 'done') RETURNING id",
    )
    .bind(user_id)
    .bind(format!("{name}-on-lastfm"))
    .fetch_one(pool)
    .await
    .unwrap();
    let imported = tracks_db::find_or_create_track(pool, artist.id, None, "Imported", None)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, import_id)
         VALUES ($1, $2, $3, $4, 'lastfm_import', $5)",
    )
    .bind(user_id)
    .bind(imported.id)
    .bind(artist.id)
    .bind(at(days_ago(200), 100))
    .bind(import_id)
    .execute(pool)
    .await
    .unwrap();
    exec(
        pool,
        "UPDATE tracks SET scrobble_count = scrobble_count + 1 WHERE id = $1",
        imported.id,
    )
    .await;
    exec(
        pool,
        "UPDATE artists SET scrobble_count = scrobble_count + 1 WHERE id = $1",
        artist.id,
    )
    .await;

    for sql in [
        "INSERT INTO user_sessions (user_id) VALUES ($1)",
        "INSERT INTO api_tokens (user_id, name, token_hash) VALUES ($1, 'ext', 'tok' || $1)",
        "INSERT INTO scrobbler_credentials (user_id, kind, name, key_hash)
         VALUES ($1, 'token', 'player', 'cred' || $1)",
        "INSERT INTO scrobbler_authorizations (token, api_key, user_id, expires_at)
         VALUES ('auth' || $1, 'key', $1, NOW() + INTERVAL '1 hour')",
        "INSERT INTO connected_accounts (user_id, provider, provider_user_id, access_token)
         VALUES ($1, 'spotify', 'sp' || $1, 'v1:secret')",
        "INSERT INTO registration_origins (user_id, key_id, address, network) VALUES ($1, 1, 2, 3)",
        "INSERT INTO coordination_days (user_id, day, factor, run_id)
         VALUES ($1, CURRENT_DATE - 3, 500, 1)",
        "INSERT INTO comments (entity_type, entity_id, user_id, body)
         SELECT 'artist', id, $1, 'great band' FROM artists LIMIT 1",
        "INSERT INTO now_playing (user_id, track_id, artist_id, expires_at)
         SELECT $1, id, artist_id, NOW() + INTERVAL '3 minutes' FROM tracks LIMIT 1",
        "UPDATE users SET similarity = 'listed', similarity_changed_at = NOW() WHERE id = $1",
        "INSERT INTO user_recommendations
             (user_id, position, artist_id, score, because_artist_id, because_count)
         SELECT $1, 1, id, 500, id, 1 FROM artists ORDER BY id LIMIT 1",
        "INSERT INTO recommendation_skips (user_id, artist_id, until)
         SELECT $1, id, NOW() + INTERVAL '90 days' FROM artists ORDER BY id LIMIT 1",
    ] {
        exec(pool, sql, user_id).await;
    }
    // A coordination run that flagged the user.
    sqlx::query(
        "INSERT INTO coordination_runs (id, fingerprint, mode, window_from, window_to, accounts,
                                        groups, flagged, cohorts, too_small, started_at, took_ms)
         VALUES ($1, 'f', 'report', CURRENT_DATE - 28, CURRENT_DATE, 10, 1, 1, 1, FALSE, NOW(), 1)
         ON CONFLICT DO NOTHING",
    )
    .bind(1)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO coordination_cohorts (run_id, cohort, kind, entity_id, members, score, factor,
                                           age, origin, client, sync, shape, burst, detail)
         VALUES (1, 1, 'artist', $1, 2, 900, 0, 0, 0, 0, 0, 0, 0, '{}') ON CONFLICT DO NOTHING",
    )
    .bind(artist.id)
    .execute(pool)
    .await
    .unwrap();
    exec(
        pool,
        "INSERT INTO coordination_accounts (run_id, user_id, cohort, suspicion, factor, age, origin,
                                            client, sync, shape, burst)
         VALUES (1, $1, 1, 900, 0, 0, 0, 0, 0, 0, 0)",
        user_id,
    )
    .await;
    // Two artwork uploads.
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO image_candidates (entity_type, entity_id, url, uploaded_by, vote_count)
             VALUES ('artist', $1, $2, $3, 1)",
        )
        .bind(artwork_artist)
        .bind(UploadKey::generate(UploadKind::ArtistImage).as_str())
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
    }
    exec(
        pool,
        "INSERT INTO image_candidate_votes (candidate_id, user_id)
         SELECT id, $1 FROM image_candidates WHERE uploaded_by = $1",
        user_id,
    )
    .await;
    user_id
}

/// Every `bigint` column called `user_id` (or `…_user_id`) in a table, view
/// or continuous aggregate, foreign key or not, and every column with a
/// foreign key to `users`, with how many rows hold `user_id`.
async fn rows_about(pool: &PgPool, user_id: i64) -> Vec<(String, i64)> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT format('%I.%I', table_schema, table_name), column_name::text
        FROM information_schema.columns
        WHERE table_schema = 'public' AND data_type = 'bigint'
          AND (column_name = 'user_id' OR column_name LIKE '%\_user\_id')
        UNION
        SELECT format('%I.%I', n.nspname, r.relname), a.attname::text
        FROM pg_constraint c
        JOIN pg_class r ON r.oid = c.conrelid
        JOIN pg_namespace n ON n.oid = r.relnamespace
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
        WHERE c.contype = 'f' AND c.confrelid = 'public.users'::regclass
          AND n.nspname = 'public'
        ORDER BY 1, 2
        "#,
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut counts = Vec::new();
    for (table, column) in columns {
        let n: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE {column} = $1"))
                .bind(user_id)
                .fetch_one(pool)
                .await
                .unwrap();
        counts.push((format!("{table}.{column}"), n));
    }
    counts
}

/// Rows of any table whose text holds `needle` (a username, an email).
async fn text_mentions(pool: &PgPool, needle: &str) -> Vec<String> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT format('%I.%I', c.table_schema, c.table_name), c.column_name::text
        FROM information_schema.columns c
        JOIN information_schema.tables t
          ON t.table_schema = c.table_schema AND t.table_name = c.table_name
        WHERE c.table_schema = 'public' AND t.table_type = 'BASE TABLE'
          AND c.data_type IN ('text', 'character varying', 'character', 'jsonb', 'json')
        "#,
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut found = Vec::new();
    for (table, column) in columns {
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE {column}::text ILIKE '%' || $1 || '%'"
        ))
        .bind(needle)
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            found.push(format!("{table}.{column}"));
        }
    }
    found
}

/// Rows a continuous aggregate has that a recount of `scrobbles` doesn't,
/// or the other way round.
async fn aggregate_drift(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        r#"
        SELECT (SELECT count(*) FROM (
                    SELECT user_id, track_id, artist_id, day, play_count FROM scrobbles_daily_by_track
                    EXCEPT
                    SELECT user_id, track_id, artist_id, time_bucket('1 day', played_at), count(*)
                    FROM scrobbles GROUP BY 1, 2, 3, 4) a)
             + (SELECT count(*) FROM (
                    SELECT user_id, track_id, artist_id, time_bucket('1 day', played_at), count(*)
                    FROM scrobbles GROUP BY 1, 2, 3, 4
                    EXCEPT
                    SELECT user_id, track_id, artist_id, day, play_count FROM scrobbles_daily_by_track) b)
             + (SELECT count(*) FROM (
                    SELECT user_id, artist_id, day, play_count FROM scrobbles_daily_by_artist
                    EXCEPT
                    SELECT user_id, artist_id, time_bucket('1 day', played_at), count(*)
                    FROM scrobbles GROUP BY 1, 2, 3) c)
             + (SELECT count(*) FROM (
                    SELECT user_id, day, scrobble_count FROM user_activity_daily
                    EXCEPT
                    SELECT user_id, time_bucket('1 day', played_at), count(*)
                    FROM scrobbles GROUP BY 1, 2) d)
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Catalog rows whose counter isn't the number of scrobbles of them.
async fn counter_drift(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        r#"
        SELECT (SELECT count(*) FROM tracks t
                WHERE t.scrobble_count <> (SELECT count(*) FROM scrobbles s WHERE s.track_id = t.id))
             + (SELECT count(*) FROM artists a
                WHERE a.scrobble_count <> (SELECT count(*) FROM scrobbles s WHERE s.artist_id = a.id))
             + (SELECT count(*) FROM albums al
                WHERE al.scrobble_count <> (SELECT count(*) FROM scrobbles s WHERE s.album_id = al.id))
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn invalidations(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM _timescaledb_catalog.continuous_aggs_hypertable_invalidation_log",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// After an erase, no table, view or aggregate has a row about the user,
/// found generically: a table added later with a `user_id` and no cascade
/// fails here instead of keeping personal data. The other user keeps
/// everything, and the shared numbers (catalog counters, aggregates,
/// rankings) are what a recount without the erased user gives.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn an_erase_leaves_no_row_about_the_user_and_corrects_shared_numbers() {
    with_db(true, |pool| async move {
        let artwork = tracks_db::find_or_create_artist(&pool, "Artwork Band")
            .await
            .unwrap()
            .id;
        let gone = populate(&pool, "erasable_qx", artwork).await;
        let stays = populate(&pool, "stays_qx", artwork).await;
        for (a, b) in [(gone, stays), (stays, gone)] {
            for sql in [
                "INSERT INTO user_follows (follower_id, followee_id) VALUES ($1, $2)",
                "INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)",
                "INSERT INTO follower_removals (user_id, follower_id, until)
                 VALUES ($1, $2, NOW() + INTERVAL '30 days')",
                "INSERT INTO user_neighbours
                     (user_id, position, neighbour_user_id, score, shared_artists, top_shared)
                 VALUES ($1, 1, $2, 500, 3, '{}')",
            ] {
                sqlx::query(sql).bind(a).bind(b).execute(&pool).await.unwrap();
            }
        }
        // Each account's say on the albums of the tracks it played.
        sqlx::query(
            "INSERT INTO track_album_votes (track_id, user_id, album_id, played_at)
             SELECT DISTINCT ON (track_id, user_id) track_id, user_id, album_id, played_at
             FROM scrobbles WHERE album_id IS NOT NULL
             ORDER BY track_id, user_id, played_at DESC",
        )
        .execute(&pool)
        .await
        .unwrap();
        // One of the erased user's uploads became the artist's image.
        sqlx::query(
            "UPDATE artists SET image_url = (SELECT min(url) FROM image_candidates WHERE uploaded_by = $1),
                                image_locked = TRUE
             WHERE id = $2",
        )
        .bind(gone)
        .bind(artwork)
        .execute(&pool)
        .await
        .unwrap();
        // The erased user likes the other's uploads.
        sqlx::query(
            "INSERT INTO image_candidate_votes (candidate_id, user_id)
             SELECT id, $2 FROM image_candidates WHERE uploaded_by = $1",
        )
        .bind(stays)
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE image_candidates SET vote_count = 2 WHERE uploaded_by = $1")
            .bind(stays)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "SELECT compress_chunk(c) FROM show_chunks('scrobbles', older_than => INTERVAL '60 days') c",
        )
        .execute(&pool)
        .await
        .unwrap();
        let classifier = cdb::register_ruleset(&pool, BudgetParams::default())
            .await
            .unwrap();
        let weights = rdb::register_ruleset(&pool, &RankingParams::default())
            .await
            .unwrap();
        let days: Vec<(i64, NaiveDate)> = sqlx::query_as(
            "SELECT DISTINCT user_id, (played_at AT TIME ZONE 'UTC')::date FROM scrobbles",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for (user_id, day) in &days {
            cdb::classify_user_day(&pool, &classifier, *user_id, *day, false)
                .await
                .unwrap();
        }
        for queued in rdb::claim_due(&pool, 1_000).await.unwrap() {
            rdb::weigh_user_day(&pool, &weights, queued.user_id, queued.day, false)
                .await
                .unwrap();
        }
        // Left queued, as the worker could have.
        cdb::enqueue_scrobble_classification(&pool, gone, Utc::now() - TimeDelta::days(4), Utc::now())
            .await
            .unwrap();
        rdb::refresh_snapshots(&pool, &weights, Period::Year, Utc::now().date_naive(), 100)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO similarity_runs
                 (fingerprint, window_from, window_to, accounts, candidates, artists, neighbours,
                  recommended_users, neighboured_users, refresh_secs, started_at, took_ms)
             VALUES ('f', CURRENT_DATE - 179, CURRENT_DATE, 2, 2, 1, 1, 2, 2, 86400, NOW(), 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        scrobbles_db::refresh_scrobble_aggregates(
            &pool,
            Utc::now() - TimeDelta::days(400),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(aggregate_drift(&pool).await, 0);
        assert_eq!(counter_drift(&pool).await, 0);
        let before = invalidations(&pool).await;

        let about_gone = rows_about(&pool, gone).await;
        for table in [
            "public.scrobbles.user_id",
            "public.scrobble_flags.user_id",
            "public.ranking_daily.user_id",
            "public.scrobbles_daily_by_track.user_id",
            "public.coordination_accounts.user_id",
            "public.user_follows.follower_id",
            "public.user_blocks.blocked_id",
            "public.follower_removals.follower_id",
            "public.track_album_votes.user_id",
            "public.user_neighbours.neighbour_user_id",
            "public.user_recommendations.user_id",
            "public.recommendation_skips.user_id",
        ] {
            let n = about_gone.iter().find(|(t, _)| t == table).map(|(_, n)| *n);
            assert!(n.is_some_and(|n| n > 0), "the fixture puts rows in {table}");
        }
        let about_stays = rows_about(&pool, stays).await;
        let stays_plays: i64 = sqlx::query_scalar("SELECT count(*) FROM scrobbles WHERE user_id = $1")
            .bind(stays)
            .fetch_one(&pool)
            .await
            .unwrap();
        let (gone_avatar, stays_votes): (String, i32) = sqlx::query_as(
            "SELECT (SELECT image_url FROM users WHERE id = $1),
                    (SELECT sum(vote_count)::int FROM image_candidates WHERE uploaded_by = $2)",
        )
        .bind(gone)
        .bind(stays)
        .fetch_one(&pool)
        .await
        .unwrap();

        let gone_voted: Vec<i64> = sqlx::query_scalar(
            "SELECT track_id FROM track_album_votes WHERE user_id = $1 ORDER BY 1",
        )
        .bind(gone)
        .fetch_all(&pool)
        .await
        .unwrap();
        accounts_db::schedule_deletion(&pool, gone, TimeDelta::zero())
            .await
            .unwrap();
        sqlx::query("DELETE FROM album_attribution_queue")
            .execute(&pool)
            .await
            .unwrap();
        let erased = accounts_db::erase(&pool, gone).await.unwrap();
        let after_erase_queue: Vec<i64> =
            sqlx::query_scalar("SELECT track_id FROM album_attribution_queue ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(erased.scrobbles, 14);
        assert!(!erased.aggregates_by_refresh);
        assert!(erased.aggregate_rows > 0);

        // Nothing left anywhere, by user id or by name.
        for (table, n) in rows_about(&pool, gone).await {
            assert_eq!(n, 0, "{table} still has rows about the erased user");
        }
        assert_eq!(text_mentions(&pool, "erasable_qx").await, Vec::<String>::new());
        // Every foreign key to users cascades (chunks of `scrobbles` included).
        let not_cascading: Vec<String> = sqlx::query_scalar(
            "SELECT conrelid::regclass::text || ' ' || conname FROM pg_constraint
             WHERE contype = 'f' AND confrelid = 'public.users'::regclass AND confdeltype <> 'c'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(not_cascading, Vec::<String>::new());

        // The other user keeps their own rows; only rows relating them to
        // the erased one (follows, blocks and removals either way, a like of
        // its upload) went.
        let after: Vec<(String, i64)> = rows_about(&pool, stays).await;
        for ((table, n), (_, m)) in about_stays.iter().zip(&after) {
            let lost = match table.as_str() {
                "public.user_follows.follower_id"
                | "public.user_follows.followee_id"
                | "public.user_blocks.blocker_id"
                | "public.user_blocks.blocked_id"
                | "public.follower_removals.user_id"
                | "public.follower_removals.follower_id"
                | "public.user_neighbours.user_id"
                | "public.user_neighbours.neighbour_user_id" => 1,
                _ => 0,
            };
            assert_eq!(n - lost, *m, "{table}");
        }
        let still: i64 = sqlx::query_scalar("SELECT count(*) FROM scrobbles WHERE user_id = $1")
            .bind(stays)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(still, stays_plays);
        let votes: i32 = sqlx::query_scalar(
            "SELECT sum(vote_count)::int FROM image_candidates WHERE uploaded_by = $1",
        )
        .bind(stays)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(votes, stays_votes - 2, "the erased user's likes are taken back");

        // Shared numbers: counters and aggregates as a recount gives them,
        // without a refresh (no invalidation was logged), and the rankings
        // due for a refresh.
        assert_eq!(counter_drift(&pool).await, 0);
        assert_eq!(aggregate_drift(&pool).await, 0);
        assert_eq!(invalidations(&pool).await, before);
        let invalidated: bool =
            sqlx::query_scalar("SELECT bool_and(invalidated) FROM ranking_snapshots")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(invalidated);
        let similarity_invalidated: bool =
            sqlx::query_scalar("SELECT bool_and(invalidated) FROM similarity_runs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(similarity_invalidated, "the similarity it counted in is due again");

        // Uploads to delete: the avatar and the artwork no artist shows. The
        // one the artist shows is the catalog's now: it stays.
        let artwork_shown: String =
            sqlx::query_scalar("SELECT image_url FROM artists WHERE id = $1")
                .bind(artwork)
                .fetch_one(&pool)
                .await
                .unwrap();
        let keys: Vec<&str> = erased.uploads.iter().map(|k| k.as_str()).collect();
        assert!(keys.contains(&gone_avatar.as_str()));
        assert!(!keys.contains(&artwork_shown.as_str()));
        assert_eq!(keys.len(), 2, "{keys:?}");

        // The tracks it had a say on are decided again without it.
        assert!(!gone_voted.is_empty());
        assert_eq!(after_erase_queue, gone_voted);

        // An erase runs again harmlessly (one that died is retried).
        let again = accounts_db::erase(&pool, gone).await.unwrap();
        assert_eq!(again.scrobbles, 0);
    })
    .await;
}

/// A request revokes every credential at once; a restore before the erase
/// begins brings the account back, and none after.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_deletion_revokes_credentials_and_can_be_taken_back_until_the_erase() {
    with_db(true, |pool| async move {
        let artwork = tracks_db::find_or_create_artist(&pool, "Artwork Band")
            .await
            .unwrap()
            .id;
        let user = populate(&pool, "leaving_qx", artwork).await;
        let import: i64 = sqlx::query_scalar(
            "INSERT INTO scrobble_imports (user_id, provider, external_user, verified)
             VALUES ($1, 'lastfm', 'x', TRUE) RETURNING id",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();

        let scheduled = accounts_db::schedule_deletion(&pool, user, TimeDelta::days(7))
            .await
            .unwrap();
        assert_eq!(scheduled.sessions.len(), 1);
        let in_a_week = Utc::now() + TimeDelta::days(7);
        assert!((scheduled.delete_after - in_a_week).abs() < TimeDelta::minutes(1));
        for table in [
            "user_sessions",
            "api_tokens",
            "scrobbler_credentials",
            "scrobbler_authorizations",
            "connected_accounts",
            "now_playing",
        ] {
            let n: i64 =
                sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE user_id = $1"))
                    .bind(user)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        let status: String =
            sqlx::query_scalar("SELECT status FROM scrobble_imports WHERE id = $1")
                .bind(import)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "cancelled");
        // The history stays for now.
        let plays: i64 = sqlx::query_scalar("SELECT count(*) FROM scrobbles WHERE user_id = $1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(plays, 14);
        // Asking again doesn't push the date back.
        let again = accounts_db::schedule_deletion(&pool, user, TimeDelta::days(30))
            .await
            .unwrap();
        assert_eq!(again.delete_after, scheduled.delete_after);
        // Not due yet.
        assert_eq!(accounts_db::claim_due(&pool).await.unwrap(), None);

        assert!(accounts_db::restore(&pool, user).await.unwrap());
        assert!(!accounts_db::restore(&pool, user).await.unwrap());

        accounts_db::schedule_deletion(&pool, user, TimeDelta::zero())
            .await
            .unwrap();
        assert_eq!(accounts_db::claim_due(&pool).await.unwrap(), Some(user));
        // Claimed: too late to restore, and nobody else takes it.
        assert!(!accounts_db::restore(&pool, user).await.unwrap());
        assert_eq!(accounts_db::claim_due(&pool).await.unwrap(), None);
    })
    .await;
}
