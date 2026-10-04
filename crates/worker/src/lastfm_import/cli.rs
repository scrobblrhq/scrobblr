//! `worker import …`: operator commands for Last.fm imports. Imports
//! started here skip the ownership check users go through in the API.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use sqlx::PgPool;

use super::{Importer, LEASE_SECS, SLICE_PAGES, SliceEnd};
use crate::enrichment::ratelimit::RateLimiter;
use db::queries::imports::{self as imports_db, NewImport};
use db::queries::users as users_db;
use shared::models::ScrobbleImport;

const CLI_PACE: Duration = Duration::from_millis(400);

pub const USAGE: &str =
    "       worker import lastfm --user NAME --lastfm LASTFM_USER [--full] [--detach]
                                  import (or resume) a Last.fm history in the foreground;
                                  --full rescans everything instead of only what's new,
                                  --detach leaves it to the running worker
       worker import status [--user NAME]
       worker import cancel ID";

pub async fn run(db: &PgPool, http: reqwest::Client, args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("lastfm") => {
            // Slower than the worker's shared limiter: a worker running
            // alongside this process makes its own Last.fm calls.
            let limiter = Arc::new(RateLimiter::new(CLI_PACE));
            let importer = Importer::from_env(db.clone(), http, limiter)?
                .context("LASTFM_API_KEY is not set")?;
            import(db, &importer, &args[1..]).await
        }
        Some("status") => status(db, &args[1..]).await,
        Some("cancel") => {
            let id: i64 = args
                .get(1)
                .context("usage: worker import cancel ID")?
                .parse()
                .context("import id must be a number")?;
            if imports_db::cancel(db, id, None).await? {
                println!("import #{id} cancelled; scrobbles already imported stay");
            } else {
                println!("import #{id} is not running");
            }
            Ok(())
        }
        _ => bail!("unknown import command (see `worker --help`)"),
    }
}

pub(super) async fn import(
    db: &PgPool,
    importer: &Importer,
    args: &[String],
) -> anyhow::Result<()> {
    let mut username = None;
    let mut lastfm_user = None;
    let (mut full, mut detach) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--user" => username = args.next().cloned(),
            "--lastfm" => lastfm_user = args.next().cloned(),
            "--full" => full = true,
            "--detach" => detach = true,
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }
    let username = username.context("--user is required")?;
    let lastfm_user = lastfm_user.context("--lastfm is required")?;
    let user = users_db::find_by_username(db, &username)
        .await?
        .with_context(|| format!("no Scrobblr user named {username}"))?;

    let id = match imports_db::active_for_user(db, user.id).await? {
        Some(active) if active.external_user.eq_ignore_ascii_case(&lastfm_user) => {
            println!("resuming import #{}", active.id);
            active.id
        }
        Some(active) => bail!(
            "{username} is already importing {} (#{}); cancel it with `worker import cancel {}`",
            active.external_user,
            active.id,
            active.id
        ),
        None => {
            let window_from = if full {
                None
            } else {
                imports_db::reimport_from(db, user.id, imports_db::PROVIDER_LASTFM, &lastfm_user)
                    .await?
            };
            let created = imports_db::create(
                db,
                &NewImport {
                    user_id: user.id,
                    provider: imports_db::PROVIDER_LASTFM,
                    external_user: &lastfm_user,
                    verified: false,
                    window_from,
                },
            )
            .await?;
            match window_from {
                Some(from) => println!(
                    "import #{} created: scrobbles since {} (`--full` rescans everything)",
                    created.id,
                    from.format("%Y-%m-%d")
                ),
                None => println!("import #{} created", created.id),
            }
            created.id
        }
    };

    if detach {
        println!("queued; the worker (`cargo run -p worker`) will run it");
        return Ok(());
    }

    loop {
        let Some(job) = imports_db::claim(db, id, LEASE_SECS).await? else {
            let current = imports_db::get(db, id, None)
                .await?
                .context("import vanished")?;
            if !current.status.is_active() {
                return finished(db, user.id, &current).await;
            }
            println!("{} (run by another worker)", progress(&current));
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let lease = job.clone();
        let end = tokio::select! {
            end = importer.process(job, SLICE_PAGES) => end?,
            _ = tokio::signal::ctrl_c() => {
                imports_db::release(db, &lease).await?;
                println!("\npaused; run the same command again to resume");
                return Ok(());
            }
        };
        let current = imports_db::get(db, id, None)
            .await?
            .context("import vanished")?;
        match end {
            SliceEnd::Deferred(until) => {
                println!(
                    "{} — retrying at {}",
                    current.error_message.as_deref().unwrap_or("Last.fm error"),
                    until.format("%H:%M:%S")
                );
                let wait = (until - chrono::Utc::now()).to_std().unwrap_or_default();
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = tokio::signal::ctrl_c() => {
                        println!("\npaused; run the same command again to resume");
                        return Ok(());
                    }
                }
            }
            SliceEnd::Paused => println!("{}", progress(&current)),
            SliceEnd::Done | SliceEnd::Failed(_) | SliceEnd::LeaseLost => {
                return finished(db, user.id, &current).await;
            }
        }
    }
}

async fn finished(db: &PgPool, user_id: i64, import: &ScrobbleImport) -> anyhow::Result<()> {
    println!("{}", progress(import));
    if let Some(message) = &import.error_message {
        println!("  {message}");
    }
    if import.imported > 0 {
        let (total, with_length) = imports_db::length_coverage(db, import.id, user_id).await?;
        println!(
            "  {with_length} of {total} imported scrobbles have a track length so far; the worker \
             (`cargo run -p worker`) fills in the rest from Last.fm and MusicBrainz and classifies \
             the imported days (`worker classify report`)"
        );
    }
    Ok(())
}

fn progress(import: &ScrobbleImport) -> String {
    let read = match import.total_expected {
        Some(total) if total > 0 => format!(
            "{} / {} read ({}%)",
            import.fetched,
            total,
            (import.fetched * 100 / total).min(100)
        ),
        _ => format!("{} read", import.fetched),
    };
    let back_to = import
        .oldest_played_at
        .map(|t| format!(" · back to {}", t.format("%Y-%m-%d")))
        .unwrap_or_default();
    format!(
        "#{} {} [{}] {read} · {} new · {} duplicate · {} skipped{back_to}",
        import.id,
        import.external_user,
        import.status.as_str(),
        import.imported,
        import.duplicates,
        import.skipped
    )
}

async fn status(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let user_id = match args {
        [] => None,
        [flag, name] if flag == "--user" => Some(
            users_db::find_by_username(db, name)
                .await?
                .with_context(|| format!("no Scrobblr user named {name}"))?
                .id,
        ),
        _ => bail!("usage: worker import status [--user NAME]"),
    };
    let imports = imports_db::list(db, user_id, 20).await?;
    if imports.is_empty() {
        println!("no imports");
    }
    for (user_id, username, import) in imports {
        let verified = if import.verified {
            "verified"
        } else {
            "operator"
        };
        println!("{username} ({verified}): {}", progress(&import));
        if import.imported > 0 {
            let (total, with_length) = imports_db::length_coverage(db, import.id, user_id).await?;
            println!(
                "  track lengths known for {with_length} of {total} imported scrobbles ({}%)",
                with_length * 100 / total.max(1)
            );
        }
        if let Some(code) = &import.error_code {
            println!(
                "  {code}: {}",
                import.error_message.as_deref().unwrap_or("")
            );
        } else if let (Some(message), Some(at)) = (&import.error_message, import.retrying_at) {
            println!(
                "  {message}; retrying at {}",
                at.format("%Y-%m-%d %H:%M:%S")
            );
        }
    }
    Ok(())
}
