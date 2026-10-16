mod common;

use common::with_db;
use db::queries::bios::{self as bios_db, Change, Written};
use db::queries::tracks as tracks_db;
use shared::community;
use shared::models::{BioRevision, CatalogKind, RevisionKind, RevisionStatus};
use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, created_at)
         VALUES ($1, $1 || '@test', 'x', NOW() - INTERVAL '30 days') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn artist(pool: &PgPool, name: &str) -> i64 {
    tracks_db::find_or_create_artist(pool, name)
        .await
        .unwrap()
        .id
}

fn change(entity_id: i64, author_id: i64, base: Option<i64>) -> Change<'static> {
    Change {
        kind: CatalogKind::Artist,
        entity_id,
        author_id,
        base,
        summary: "",
    }
}

async fn edit(pool: &PgPool, c: &Change<'_>, body: &str) -> BioRevision {
    match bios_db::edit(pool, c, body).await.unwrap() {
        Written::Revision(r) => *r,
        other => panic!("not written: {other:?}"),
    }
}

async fn mirror(pool: &PgPool, artist_id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT bio FROM artists WHERE id = $1")
        .bind(artist_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Edits build a history over their base; a stale base is a conflict; a
/// revert restores an earlier text as a new revision; `artists.bio` follows
/// the current text throughout.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn edits_and_reverts_build_the_history() {
    with_db(true, |pool| async move {
        let (alice, bob) = (user(&pool, "alice").await, user(&pool, "bob").await);
        let band = artist(&pool, "Band").await;

        let first = edit(&pool, &change(band, alice, None), "Formed in 1990.").await;
        assert_eq!(first.status, RevisionStatus::Published);
        assert_eq!(first.parent_id, None);
        assert!(first.current);
        assert_eq!(
            mirror(&pool, band).await.as_deref(),
            Some("Formed in 1990.")
        );

        let second = edit(&pool, &change(band, bob, Some(first.id)), "Formed in 1991.").await;
        assert_eq!(second.parent_id, Some(first.id));

        // Alice still edits over the first: refused, told the current one.
        let stale = bios_db::edit(
            &pool,
            &change(band, alice, Some(first.id)),
            "Formed in 1989.",
        )
        .await
        .unwrap();
        assert!(
            matches!(stale, Written::Conflict { current: Some(id) } if id == second.id),
            "{stale:?}"
        );
        let same = bios_db::edit(
            &pool,
            &change(band, alice, Some(second.id)),
            "Formed in 1991.",
        )
        .await
        .unwrap();
        assert!(matches!(same, Written::Unchanged), "{same:?}");

        let Written::Revision(back) =
            bios_db::revert(&pool, &change(band, alice, Some(second.id)), first.id)
                .await
                .unwrap()
        else {
            panic!("not reverted");
        };
        assert_eq!(back.kind, RevisionKind::Revert);
        assert_eq!(back.reverted_to, Some(first.id));
        assert_eq!(back.parent_id, Some(second.id));
        assert_eq!(back.body.as_deref(), Some("Formed in 1990."));
        assert_eq!(
            mirror(&pool, band).await.as_deref(),
            Some("Formed in 1990.")
        );
        // Reverting to the text it already has changes nothing.
        let again = bios_db::revert(&pool, &change(band, bob, Some(back.id)), first.id)
            .await
            .unwrap();
        assert!(matches!(again, Written::Unchanged), "{again:?}");

        let current = bios_db::current(&pool, CatalogKind::Artist, band, None)
            .await
            .unwrap();
        assert_eq!(current.revision_id, Some(back.id));
        assert_eq!(current.revisions, 3);
        assert_eq!(current.author.unwrap().username, "alice");

        let history = bios_db::history(&pool, CatalogKind::Artist, band, None, 2, None)
            .await
            .unwrap();
        let ids: Vec<i64> = history.iter().map(|r| r.id).collect();
        assert_eq!(ids, [back.id, second.id]);
        assert!(history[0].current && !history[1].current);
        assert!(history.iter().all(|r| r.body.is_none()));
        assert_eq!(history[1].length, 15);
        let rest = bios_db::history(&pool, CatalogKind::Artist, band, Some(second.id), 2, None)
            .await
            .unwrap();
        assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), [first.id]);

        // Albums have bios too, and no artist mirror follows them.
        let album = tracks_db::find_or_create_album(&pool, band, "Album")
            .await
            .unwrap();
        let mut on_album = change(album, bob, None);
        on_album.kind = CatalogKind::Album;
        edit(&pool, &on_album, "Their second album.").await;
        assert_eq!(
            mirror(&pool, band).await.as_deref(),
            Some("Formed in 1990.")
        );
    })
    .await;
}

/// A revert's target is a published, visible revision of the same bio.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn reverts_only_to_visible_revisions_of_the_same_bio() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let (one, two) = (artist(&pool, "One").await, artist(&pool, "Two").await);
        let a = edit(&pool, &change(one, alice, None), "One's first.").await;
        let b = edit(&pool, &change(one, alice, Some(a.id)), "One's second.").await;
        let other = edit(&pool, &change(two, alice, None), "Two's.").await;

        let refused = bios_db::revert(&pool, &change(one, alice, Some(b.id)), other.id)
            .await
            .unwrap();
        assert!(matches!(refused, Written::BadTarget), "{refused:?}");
        sqlx::query("UPDATE bio_revisions SET hidden_at = NOW() WHERE id = $1")
            .bind(a.id)
            .execute(&pool)
            .await
            .unwrap();
        let hidden = bios_db::revert(&pool, &change(one, alice, Some(b.id)), a.id)
            .await
            .unwrap();
        assert!(matches!(hidden, Written::BadTarget), "{hidden:?}");
        let history = bios_db::history(&pool, CatalogKind::Artist, one, None, 10, None)
            .await
            .unwrap();
        let shown = history.iter().find(|r| r.id == a.id).unwrap();
        assert!(shown.hidden && shown.summary.is_none());
    })
    .await;
}

/// A locked bio takes no edit; an author's revisions are capped per bio
/// and per day.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn locks_and_caps_stop_edit_wars() {
    with_db(true, |pool| async move {
        let (alice, bob) = (user(&pool, "alice").await, user(&pool, "bob").await);
        let band = artist(&pool, "Band").await;
        let mut base = None;
        for i in 0..community::EDITS_PER_BIO_PER_DAY {
            base = Some(edit(&pool, &change(band, alice, base), &format!("Take {i}.")).await.id);
        }
        let capped = bios_db::edit(&pool, &change(band, alice, base), "One more.")
            .await
            .unwrap();
        assert!(matches!(capped, Written::BioCap), "{capped:?}");
        // Someone else still may.
        let bobs = edit(&pool, &change(band, bob, base), "Bob's take.").await;

        sqlx::query(
            "UPDATE entity_bios SET locked_at = NOW() WHERE entity_type = 'artist' AND entity_id = $1",
        )
        .bind(band)
        .execute(&pool)
        .await
        .unwrap();
        let locked = bios_db::edit(&pool, &change(band, bob, Some(bobs.id)), "Locked out.")
            .await
            .unwrap();
        assert!(matches!(locked, Written::Locked), "{locked:?}");
        let locked = bios_db::revert(&pool, &change(band, bob, Some(bobs.id)), base.unwrap())
            .await
            .unwrap();
        assert!(matches!(locked, Written::Locked), "{locked:?}");

        // The day's cap, over many bios.
        sqlx::query(
            "INSERT INTO bio_revisions (entity_type, entity_id, author_id, kind, body, status)
             SELECT 'artist', -g, $1, 'edit', 'x', 'published' FROM generate_series(1, $2) g",
        )
        .bind(bob)
        .bind(community::EDITS_PER_DAY as i32)
        .execute(&pool)
        .await
        .unwrap();
        let other = artist(&pool, "Other").await;
        let capped = bios_db::edit(&pool, &change(other, bob, None), "Too many today.")
            .await
            .unwrap();
        assert!(matches!(capped, Written::DailyCap), "{capped:?}");
    })
    .await;
}

/// Spam waits for review: a link repeated in one text, a host pasted into
/// other bios, a long text copied from another bio. Held edits leave the
/// bio as it was, and only their author sees them.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn spam_is_held_for_review() {
    with_db(true, |pool| async move {
        let (alice, bob) = (user(&pool, "alice").await, user(&pool, "bob").await);
        let mut bands = Vec::new();
        for i in 0..8 {
            bands.push(artist(&pool, &format!("Band {i}")).await);
        }

        let held = edit(
            &pool,
            &change(bands[0], alice, None),
            "Buy at https://shop.example/a, https://shop.example/b and https://shop.example/c",
        )
        .await;
        assert_eq!(held.status, RevisionStatus::Held);
        assert_eq!(held.held_reason.as_deref(), Some("repeated_link"));
        assert!(!held.current);
        assert_eq!(mirror(&pool, bands[0]).await, None);
        let mut conn = pool.acquire().await.unwrap();
        assert!(
            bios_db::revision(&mut conn, held.id, Some(bob))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            bios_db::revision(&mut conn, held.id, Some(alice))
                .await
                .unwrap()
                .is_some()
        );
        let current = bios_db::current(&pool, CatalogKind::Artist, bands[0], None)
            .await
            .unwrap();
        assert_eq!((current.revision_id, current.revisions), (None, 0));

        // The same host into a third bio by its author within a week.
        for band in &bands[1..3] {
            let r = edit(
                &pool,
                &change(*band, alice, None),
                "Site: https://fan.example",
            )
            .await;
            assert_eq!(r.status, RevisionStatus::Published);
        }
        let third = edit(
            &pool,
            &change(bands[3], alice, None),
            "Site: https://fan.example/x",
        )
        .await;
        assert_eq!(third.held_reason.as_deref(), Some("link_across_bios"));
        // A host already in the text isn't added by the edit.
        let base = bios_db::current(&pool, CatalogKind::Artist, bands[1], None)
            .await
            .unwrap()
            .revision_id;
        let kept = edit(
            &pool,
            &change(bands[1], alice, base),
            "Site: https://fan.example (official).",
        )
        .await;
        assert_eq!(kept.status, RevisionStatus::Published);

        // A long text copied from another bio.
        let long = format!("{} The end.", "A long story about a band. ".repeat(10));
        let original = edit(&pool, &change(bands[4], bob, None), &long).await;
        assert_eq!(original.status, RevisionStatus::Published);
        let copy = edit(
            &pool,
            &change(bands[5], bob, None),
            &long.to_uppercase().replace(' ', "  "),
        )
        .await;
        assert_eq!(copy.held_reason.as_deref(), Some("duplicate_text"));
        // Short texts are alike too often to count.
        let short = edit(
            &pool,
            &change(bands[6], bob, None),
            "Site: https://fan.example",
        )
        .await;
        assert_eq!(
            short.status,
            RevisionStatus::Published,
            "another author, once"
        );
    })
    .await;
}

/// Who may edit: accounts 7 days old, not suspended, not flagged by the
/// coordination detection below half weight.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn editors_are_read_from_the_account() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let now = chrono::Utc::now();
        let editor = bios_db::editor(&pool, alice).await.unwrap().unwrap();
        assert_eq!(editor.refusal(now, shared::ranking::DEFAULT_MIN_ACCOUNT_DAYS), None);

        sqlx::query(
            "INSERT INTO coordination_days (user_id, day, factor, run_id) VALUES
             ($1, CURRENT_DATE - 40, 0, 1), ($1, CURRENT_DATE - 3, 700, 1)",
        )
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();
        let editor = bios_db::editor(&pool, alice).await.unwrap().unwrap();
        assert_eq!(editor.lowest_factor, Some(700), "outside the window doesn't count");
        assert_eq!(editor.refusal(now, shared::ranking::DEFAULT_MIN_ACCOUNT_DAYS), None);
        sqlx::query("UPDATE coordination_days SET factor = 300 WHERE user_id = $1 AND day > CURRENT_DATE - 10")
            .bind(alice)
            .execute(&pool)
            .await
            .unwrap();
        let editor = bios_db::editor(&pool, alice).await.unwrap().unwrap();
        assert_eq!(editor.refusal(now, shared::ranking::DEFAULT_MIN_ACCOUNT_DAYS), Some(community::Refusal::NotNow));

        let bob = user(&pool, "bob").await;
        sqlx::query("INSERT INTO editing_suspensions (user_id, until) VALUES ($1, NOW() - INTERVAL '1 hour')")
            .bind(bob)
            .execute(&pool)
            .await
            .unwrap();
        let editor = bios_db::editor(&pool, bob).await.unwrap().unwrap();
        assert!(!editor.suspended, "a suspension that ended");
    })
    .await;
}

/// Last.fm's bio is the first revision, replaced by a forced refresh only
/// while nobody has edited it; never once someone has.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn imports_never_overwrite_people() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let band = artist(&pool, "Band").await;
        assert!(
            bios_db::import(&pool, band, " From Last.fm. ", false)
                .await
                .unwrap()
        );
        assert_eq!(mirror(&pool, band).await.as_deref(), Some("From Last.fm."));
        assert!(
            !bios_db::import(&pool, band, "Newer.", false).await.unwrap(),
            "fill only"
        );
        assert!(bios_db::import(&pool, band, "Newer.", true).await.unwrap());
        let current = bios_db::current(&pool, CatalogKind::Artist, band, None)
            .await
            .unwrap();
        assert_eq!(
            (current.kind, current.author.is_none()),
            (Some(RevisionKind::Import), true)
        );

        let edited = edit(
            &pool,
            &change(band, alice, current.revision_id),
            "By a person.",
        )
        .await;
        assert!(
            !bios_db::import(&pool, band, "Even newer.", true)
                .await
                .unwrap()
        );
        // Reverting to the import doesn't hand the bio back to Last.fm.
        let first = bios_db::history(&pool, CatalogKind::Artist, band, None, 10, None)
            .await
            .unwrap()
            .last()
            .unwrap()
            .id;
        bios_db::revert(&pool, &change(band, alice, Some(edited.id)), first)
            .await
            .unwrap();
        assert!(
            !bios_db::import(&pool, band, "Even newer.", true)
                .await
                .unwrap()
        );
        assert_eq!(mirror(&pool, band).await.as_deref(), Some("From Last.fm."));
        // Nor over a moderator: a hidden import (which leaves no text) or
        // a lock keeps Last.fm out.
        let hidden = artist(&pool, "Hidden").await;
        assert!(
            bios_db::import(&pool, hidden, "Someone's address.", false)
                .await
                .unwrap()
        );
        let first = bios_db::current(&pool, CatalogKind::Artist, hidden, None)
            .await
            .unwrap()
            .revision_id
            .unwrap();
        db::queries::moderation::hide(&pool, first).await.unwrap();
        assert_eq!(mirror(&pool, hidden).await, None);
        assert!(
            !bios_db::import(&pool, hidden, "Someone's address.", true)
                .await
                .unwrap()
        );
        assert!(
            !bios_db::import(&pool, hidden, "Something else.", false)
                .await
                .unwrap()
        );
        let locked = artist(&pool, "Locked").await;
        assert!(
            bios_db::import(&pool, locked, "First.", false)
                .await
                .unwrap()
        );
        db::queries::moderation::lock(&pool, CatalogKind::Artist, locked, Some("war"))
            .await
            .unwrap();
        assert!(
            !bios_db::import(&pool, locked, "Second.", true)
                .await
                .unwrap()
        );
        // Text the rules refuse isn't imported.
        let other = artist(&pool, "Other").await;
        assert!(
            !bios_db::import(&pool, other, "bidi\u{202E}", false)
                .await
                .unwrap()
        );
    })
    .await;
}
