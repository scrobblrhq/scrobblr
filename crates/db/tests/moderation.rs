mod common;

use chrono::Utc;
use common::with_db;
use db::queries::bios::{self as bios_db, Change, Written};
use db::queries::moderation::{self as moderation_db, Moderated};
use db::queries::tags::{self as tags_db, Added, TagSource, Weighing};
use db::queries::tracks as tracks_db;
use shared::community::{self, Refusal};
use shared::models::{BioRevision, CatalogKind, RevisionKind, RevisionStatus};
use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, created_at)
         VALUES ($1, $1 || '@test', 'x', NOW() - INTERVAL '60 days') RETURNING id",
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

async fn head(pool: &PgPool, band: i64) -> Option<i64> {
    bios_db::current(pool, CatalogKind::Artist, band, None)
        .await
        .unwrap()
        .revision_id
}

async fn edit(pool: &PgPool, author: i64, band: i64, body: &str) -> Written {
    let base = head(pool, band).await;
    let change = Change {
        kind: CatalogKind::Artist,
        entity_id: band,
        author_id: author,
        base,
        summary: "",
    };
    bios_db::edit(pool, &change, body).await.unwrap()
}

fn revision(w: Written) -> BioRevision {
    match w {
        Written::Revision(r) => *r,
        other => panic!("not written: {other:?}"),
    }
}

async fn shown(pool: &PgPool, band: i64) -> Option<String> {
    bios_db::current(pool, CatalogKind::Artist, band, None)
        .await
        .unwrap()
        .body
}

async fn recount(pool: &PgPool) {
    sqlx::query("UPDATE tag_queue SET due_at = NOW()")
        .execute(pool)
        .await
        .unwrap();
    while tags_db::recount_due(pool, Weighing::at(Utc::now()), 100)
        .await
        .unwrap()
        > 0
    {}
}

async fn visible_tags(pool: &PgPool, band: i64) -> Vec<String> {
    tags_db::top_tags(
        pool,
        CatalogKind::Artist,
        band,
        5 * shared::ranking::FULL,
        10,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(name, _)| name)
    .collect()
}

/// The abuse scenario: a vandal and a farm of six aged, unflagged accounts
/// push a bad bio over an honest one and a spam tag past the minimum. One
/// operator command (suspend them all with --undo, then recount) puts the
/// honest bio back and takes the tag off the visible state; the history
/// keeps everything, and the accounts can neither edit nor weigh any more.
/// What the attackers could do before that is bounded by the caps: three
/// revisions of a bio a day each, links pasted across bios held.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn one_command_undoes_a_farm_and_a_vandal() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let vandal = user(&pool, "vandal").await;
        let mut farm = Vec::new();
        for i in 0..6 {
            farm.push(user(&pool, &format!("farm{i}")).await);
        }
        let band = artist(&pool, "Band").await;
        let honest = revision(edit(&pool, alice, band, "An honest bio.").await);
        for fan in 0..5 {
            let fan = user(&pool, &format!("fan{fan}")).await;
            tags_db::add(
                &pool,
                fan,
                CatalogKind::Artist,
                band,
                &["shoegaze".into()],
                TagSource::NATIVE,
            )
            .await
            .unwrap();
        }

        // The vandal's text is published (no link, nothing copied)...
        revision(
            edit(
                &pool,
                vandal,
                band,
                "This band is terrible, buy pills instead.",
            )
            .await,
        );
        // ...its link spam across the catalog is held from the third bio on.
        let mut held = 0;
        for i in 0..4 {
            let other = artist(&pool, &format!("Other {i}")).await;
            let r = revision(edit(&pool, vandal, other, "Visit https://pills.example now").await);
            held += usize::from(r.status == RevisionStatus::Held);
        }
        assert_eq!(held, 2);
        // The farm keeps the bad text up in turns, three a day each at most.
        for (i, member) in farm.iter().enumerate() {
            let text = format!("This band is terrible ({i}), buy pills instead.");
            revision(edit(&pool, *member, band, &text).await);
        }
        revision(edit(&pool, farm[0], band, "Again, buy pills.").await);
        revision(edit(&pool, farm[0], band, "Once more, buy pills.").await);
        assert!(matches!(
            edit(&pool, farm[0], band, "A fourth time.").await,
            Written::BioCap
        ));
        // ...and tags the band.
        for member in &farm {
            let added = tags_db::add(
                &pool,
                *member,
                CatalogKind::Artist,
                band,
                &["buy pills".into()],
                TagSource::NATIVE,
            )
            .await
            .unwrap();
            assert!(matches!(added, Added::Tags(_)));
        }
        recount(&pool).await;
        assert_eq!(visible_tags(&pool, band).await, ["buy pills", "shoegaze"]);
        assert_ne!(shown(&pool, band).await.as_deref(), Some("An honest bio."));
        let revisions_before: i64 = sqlx::query_scalar("SELECT count(*) FROM bio_revisions")
            .fetch_one(&pool)
            .await
            .unwrap();

        // The operator's one command.
        let mut accounts = farm.clone();
        accounts.push(vandal);
        let undone = moderation_db::suspend(&pool, &accounts, None, "farm", true)
            .await
            .unwrap();
        assert_eq!(
            undone.bios, 3,
            "the band and the two bios the vandal's links reached"
        );
        assert_eq!(undone.rejected, 2);
        recount(&pool).await;

        assert_eq!(shown(&pool, band).await.as_deref(), Some("An honest bio."));
        let current = bios_db::current(&pool, CatalogKind::Artist, band, None)
            .await
            .unwrap();
        let restored = sqlx::query_as::<_, (String, Option<i64>)>(
            "SELECT kind, reverted_to FROM bio_revisions WHERE id = $1",
        )
        .bind(current.revision_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(restored, ("moderation".into(), Some(honest.id)));
        assert_eq!(visible_tags(&pool, band).await, ["shoegaze"]);
        let revisions_after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM bio_revisions WHERE kind <> 'moderation'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            revisions_after, revisions_before,
            "the history keeps everything"
        );
        for account in &accounts {
            let editor = bios_db::editor(&pool, *account).await.unwrap().unwrap();
            assert_eq!(editor.refusal(Utc::now()), Some(Refusal::NotNow));
        }

        // Lifting the suspension gives their tags their weight back.
        moderation_db::unsuspend(&pool, &farm).await.unwrap();
        recount(&pool).await;
        assert_eq!(visible_tags(&pool, band).await, ["buy pills", "shoegaze"]);
        // Blocking the term takes it off everything and refuses it anew.
        let gone = moderation_db::block_term(&pool, &community::tag_name("PILLS").unwrap())
            .await
            .unwrap();
        assert_eq!(gone, 6);
        recount(&pool).await;
        assert_eq!(visible_tags(&pool, band).await, ["shoegaze"]);
        let terms = tags_db::blocklist(&pool).await.unwrap();
        assert!(community::is_blocked("cheap pills", &terms));
    })
    .await;
}

/// Hiding a revision hides its copies and puts the bio back to its latest
/// visible text; a lock stops everyone but a moderator; a held edit is
/// published over the revision it was written on, or refused as stale.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn hiding_locking_and_deciding_held_edits() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let band = artist(&pool, "Band").await;
        let first = revision(edit(&pool, alice, band, "First.").await);
        let doxx = revision(edit(&pool, bob, band, "Someone's home address.").await);
        revision(edit(&pool, alice, band, "Third.").await);
        let change = Change {
            kind: CatalogKind::Artist,
            entity_id: band,
            author_id: bob,
            base: head(&pool, band).await,
            summary: "",
        };
        // Bob restores his text with a revert: a copy of it.
        revision(bios_db::revert(&pool, &change, doxx.id).await.unwrap());

        assert_eq!(moderation_db::hide(&pool, doxx.id).await.unwrap(), Some(2));
        assert_eq!(shown(&pool, band).await.as_deref(), Some("Third."));
        let history = bios_db::history(&pool, CatalogKind::Artist, band, None, 10, None)
            .await
            .unwrap();
        assert_eq!(history.iter().filter(|r| r.hidden).count(), 2);
        assert_eq!(history[0].kind, RevisionKind::Moderation);

        moderation_db::lock(&pool, CatalogKind::Artist, band, Some("edit war"))
            .await
            .unwrap();
        assert!(matches!(
            edit(&pool, alice, band, "Locked out.").await,
            Written::Locked
        ));
        assert_eq!(
            moderation_db::revert(&pool, CatalogKind::Artist, band, first.id, "back")
                .await
                .unwrap(),
            Moderated::Done
        );
        assert_eq!(shown(&pool, band).await.as_deref(), Some("First."));
        assert_eq!(
            moderation_db::revert(&pool, CatalogKind::Artist, band, doxx.id, "no")
                .await
                .unwrap(),
            Moderated::NoSuchRevision,
            "hidden text can't come back"
        );
        moderation_db::lock(&pool, CatalogKind::Artist, band, None)
            .await
            .unwrap();

        let other = artist(&pool, "Other").await;
        let spam = "https://a.example https://a.example https://a.example";
        let held = revision(edit(&pool, alice, other, spam).await);
        assert_eq!(held.status, RevisionStatus::Held);
        revision(edit(&pool, bob, other, "Meanwhile.").await);
        assert_eq!(
            moderation_db::decide(&pool, held.id, true, false)
                .await
                .unwrap(),
            Moderated::Stale
        );
        assert_eq!(
            moderation_db::decide(&pool, held.id, true, true)
                .await
                .unwrap(),
            Moderated::Done
        );
        assert_eq!(shown(&pool, other).await.as_deref(), Some(spam));
        assert_eq!(
            moderation_db::decide(&pool, held.id, false, false)
                .await
                .unwrap(),
            Moderated::NoSuchRevision,
            "decided already"
        );
        // A held copy of hidden text can't be published.
        let copy = revision(
            edit(
                &pool,
                alice,
                other,
                "https://b.example https://b.example https://b.example",
            )
            .await,
        );
        assert_eq!(copy.status, RevisionStatus::Held);
        moderation_db::hide(&pool, copy.id).await.unwrap();
        assert_eq!(
            moderation_db::decide(&pool, copy.id, true, true)
                .await
                .unwrap(),
            Moderated::Hidden
        );
        assert!(moderation_db::held(&pool, 10).await.unwrap()[0].hidden);
    })
    .await;
}

/// Reports queue once per reporter and target, and close.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn reports_queue_and_close() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice").await;
        let band = artist(&pool, "Band").await;
        let r = revision(edit(&pool, alice, band, "Text.").await);
        let target = moderation_db::Target::Revision(r.id);
        assert!(
            moderation_db::report(&pool, alice, target, "spam", "")
                .await
                .unwrap()
        );
        assert!(
            !moderation_db::report(&pool, alice, target, "other", "")
                .await
                .unwrap()
        );
        let open = moderation_db::open_reports(&pool, 10).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].author.as_deref(), open[0].excerpt.as_deref()),
            (Some("alice"), Some("Text."))
        );
        assert_eq!(
            moderation_db::resolve(&pool, &[open[0].id], false, "fine")
                .await
                .unwrap(),
            1
        );
        assert!(
            moderation_db::open_reports(&pool, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            moderation_db::report(&pool, alice, target, "spam", "again")
                .await
                .unwrap()
        );
    })
    .await;
}
