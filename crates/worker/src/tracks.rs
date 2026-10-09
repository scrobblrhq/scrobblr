//! `worker tracks mb-review`: MusicBrainz matches whose length two other
//! sources contradict (`shared::track_lengths`), and with `--apply` takes
//! them back. A track's mbid is otherwise never overwritten.

use anyhow::{Context, bail};
use sqlx::PgPool;

use db::queries::enrichment as edb;
use shared::track_lengths::{Outlier, outlier};

pub const USAGE: &str = "       worker tracks mb-review [--limit N] [--apply]
                               list tracks whose MusicBrainz length the catalog and Deezer
                               both contradict (a snippet, live take or medley matched);
                               --apply unlinks those matches (default: list only)";

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    if args.first().map(String::as_str) != Some("mb-review") {
        bail!("unknown tracks command (see `worker --help`)");
    }
    let mut apply = false;
    let mut limit = 500;
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apply" => apply = true,
            "--dry-run" => apply = false,
            "--limit" => {
                limit = args
                    .next()
                    .context("--limit needs a number")?
                    .parse()
                    .context("--limit: expected a number")?
            }
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }

    let disputed = edb::disputed_lengths(db, limit).await?;
    let mut flagged = 0;
    let mut others = Vec::new();
    for t in &disputed {
        let found = outlier(
            t.musicbrainz_ms.into(),
            t.catalog_ms.into(),
            t.deezer_ms.into(),
        );
        if found != Some(Outlier::MusicBrainz) {
            others.push((t, found));
            continue;
        }
        flagged += 1;
        let action = if apply {
            match edb::unlink_musicbrainz(db, t.track_id, t.musicbrainz_ms).await? {
                Some(days) => format!("unlinked; {days} days queued for classification"),
                None => "changed since listed; left alone".into(),
            }
        } else {
            "would unlink".into()
        };
        println!(
            "#{:<8} {} — {}: musicbrainz {} vs catalog {}, deezer {} ({} scrobbles, mbid {}): {action}",
            t.track_id,
            t.artist_name,
            t.title,
            duration(t.musicbrainz_ms),
            duration(t.catalog_ms),
            duration(t.deezer_ms),
            t.scrobble_count,
            t.mbid.map_or("-".into(), |m| m.to_string()),
        );
    }

    if !others.is_empty() {
        println!("\nother disagreements (left alone):");
        for (t, found) in others {
            let verdict = found.map_or("no two sources agree".into(), |o| {
                format!("{} is the odd one out", o.as_str())
            });
            println!(
                "  #{:<8} {} — {}: musicbrainz {}, catalog {}, deezer {}: {verdict}",
                t.track_id,
                t.artist_name,
                t.title,
                duration(t.musicbrainz_ms),
                duration(t.catalog_ms),
                duration(t.deezer_ms),
            );
        }
    }
    println!(
        "\n{} tracks with three lengths in dispute; {flagged} MusicBrainz matches {}",
        disputed.len(),
        if apply {
            "taken back"
        } else {
            "to take back (run with --apply)"
        }
    );
    Ok(())
}

fn duration(ms: i32) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}
