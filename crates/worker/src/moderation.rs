//! `worker moderation`: the operator's side of reports, bios and tags
//! (`db::queries::moderation`; docs/operations.md, Moderation). Commands
//! changing tags recount what they touched before returning.

use anyhow::{Context, bail};
use chrono::{TimeDelta, Utc};
use sqlx::PgPool;

use db::queries::moderation::{self as moderation_db, Moderated};
use db::queries::{tags as tags_db, users as users_db};
use shared::community;
use shared::models::CatalogKind;

pub const USAGE: &str = "       worker moderation reports [--limit N]
                                  open reports, oldest first, with what they are about
       worker moderation resolve ID... [--dismiss] [--note TEXT]
                                  close reports as dealt with (or dismissed)
       worker moderation held [--limit N]
                                  edits held for review as possible spam
       worker moderation approve REVISION [--force]
                                  publish a held edit (--force: even over a newer revision)
       worker moderation reject REVISION
       worker moderation revert artist|album ID --to REVISION [--note TEXT]
                                  put an earlier revision's text back, lock or not
       worker moderation lock artist|album ID --reason TEXT
       worker moderation unlock artist|album ID
                                  stop (or allow again) edits and reverts of a bio
       worker moderation hide REVISION
                                  hide a revision's text everywhere in its bio's history;
                                  a bio showing it goes back to its latest visible revision
       worker moderation unhide REVISION
       worker moderation suspend NAME... [--days N] [--reason TEXT] [--undo]
                                  stop accounts editing bios and tagging (for N days, or
                                  until lifted); their tags stop counting at once. --undo
                                  puts every bio showing their text back to its latest
                                  revision by an account not suspended, and rejects their
                                  held edits
       worker moderation unsuspend NAME...
       worker moderation untag TAG [--artist ID | --album ID | --track ID]
                                  take a tag off one entity, or everything, for everyone
       worker moderation block-tag TERM
                                  refuse TERM's words in new tags and take the tags
                                  holding them off everything
       worker moderation unblock-tag TERM
       worker moderation blocklist";

fn kind(arg: Option<&String>) -> anyhow::Result<CatalogKind> {
    match arg.map(String::as_str) {
        Some("artist") => Ok(CatalogKind::Artist),
        Some("album") => Ok(CatalogKind::Album),
        _ => bail!("expected `artist` or `album`"),
    }
}

fn number(arg: Option<&String>, what: &str) -> anyhow::Result<i64> {
    arg.and_then(|a| a.parse().ok())
        .with_context(|| format!("expected {what}"))
}

/// The value after `--name`, if given.
fn option<'a>(args: &'a [String], name: &str) -> Option<&'a String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
}

/// The arguments that aren't options or option values.
fn plain<'a>(args: &'a [String], with_values: &[&str]) -> Vec<&'a String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if with_values.contains(&a.as_str()) {
            skip = true;
        } else if !a.starts_with("--") {
            out.push(a);
        }
    }
    out
}

async fn user_ids(db: &PgPool, names: &[&String]) -> anyhow::Result<Vec<i64>> {
    if names.is_empty() {
        bail!("name at least one account");
    }
    let mut ids = Vec::new();
    for name in names {
        let user = users_db::find_by_username(db, name)
            .await?
            .with_context(|| format!("no user named `{name}`"))?;
        ids.push(user.id);
    }
    Ok(ids)
}

fn moderated(result: Moderated, done: &str) -> anyhow::Result<()> {
    match result {
        Moderated::Done => {
            println!("{done}");
            Ok(())
        }
        Moderated::NoSuchRevision => {
            bail!("no such revision of that bio (published and visible, or held)")
        }
        Moderated::Stale => bail!(
            "the bio changed since the edit was written: reject it, or approve it with --force \
             to write it over the newer text"
        ),
    }
}

async fn recount(db: &PgPool) -> anyhow::Result<()> {
    let n = crate::tags::drain(db).await?;
    println!("recounted the tags of {n} entities");
    Ok(())
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let rest = &args[1.min(args.len())..];
    let limit = || {
        option(rest, "--limit")
            .and_then(|n| n.parse().ok())
            .unwrap_or(50)
    };
    match args.first().map(String::as_str) {
        Some("reports") => {
            let reports = moderation_db::open_reports(db, limit()).await?;
            if reports.is_empty() {
                println!("no open report");
            }
            for r in reports {
                let target = match (&r.revision_id, &r.tag) {
                    (Some(rev), _) => format!(
                        "revision {rev} of {} {} by {}: {}",
                        r.entity_type.as_deref().unwrap_or("?"),
                        r.entity_id.unwrap_or(0),
                        r.author.as_deref().unwrap_or("(nobody)"),
                        r.excerpt.as_deref().unwrap_or("")
                    ),
                    (None, Some(tag)) => match (&r.entity_type, r.entity_id) {
                        (Some(t), Some(id)) => format!("tag `{tag}` on {t} {id}"),
                        _ => format!("tag `{tag}` anywhere"),
                    },
                    _ => "?".into(),
                };
                println!(
                    "#{:<6} {}  {:<13} by {:<20} ({} on this)  {target}{}",
                    r.id,
                    r.created_at.format("%Y-%m-%d %H:%M"),
                    r.reason,
                    r.reporter,
                    r.on_target,
                    if r.note.is_empty() {
                        String::new()
                    } else {
                        format!("\n         note: {}", r.note)
                    }
                );
            }
            Ok(())
        }
        Some("resolve") => {
            let ids = plain(rest, &["--note"])
                .iter()
                .map(|a| a.parse::<i64>().context("expected report ids"))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let note = option(rest, "--note").map_or("", String::as_str);
            let n = moderation_db::resolve(db, &ids, rest.iter().any(|a| a == "--dismiss"), note)
                .await?;
            println!("closed {n} report(s)");
            Ok(())
        }
        Some("held") => {
            for h in moderation_db::held(db, limit()).await? {
                println!(
                    "{:<8} {}  {} {:<8} by {:<20} {:<17} {}",
                    h.id,
                    h.created_at.format("%Y-%m-%d %H:%M"),
                    h.entity_type,
                    h.entity_id,
                    h.author.as_deref().unwrap_or("(nobody)"),
                    h.held_reason.as_deref().unwrap_or(""),
                    h.excerpt
                );
            }
            Ok(())
        }
        Some(cmd @ ("approve" | "reject")) => {
            let revision = number(rest.first(), "a revision id")?;
            let approve = cmd == "approve";
            let force = rest.iter().any(|a| a == "--force");
            moderated(
                moderation_db::decide(db, revision, approve, force).await?,
                if approve { "published" } else { "rejected" },
            )
        }
        Some("revert") => {
            let kind = kind(rest.first())?;
            let id = number(rest.get(1), "an id")?;
            let to = number(option(rest, "--to"), "--to REVISION")?;
            let note = option(rest, "--note").map_or("reverted by a moderator", String::as_str);
            moderated(
                moderation_db::revert(db, kind, id, to, note).await?,
                "reverted",
            )
        }
        Some(cmd @ ("lock" | "unlock")) => {
            let kind = kind(rest.first())?;
            let id = number(rest.get(1), "an id")?;
            let reason = match cmd {
                "lock" => Some(
                    option(rest, "--reason")
                        .context("a lock needs --reason")?
                        .as_str(),
                ),
                _ => None,
            };
            moderation_db::lock(db, kind, id, reason).await?;
            println!("{cmd}ed");
            Ok(())
        }
        Some("hide") => {
            let revision = number(rest.first(), "a revision id")?;
            match moderation_db::hide(db, revision).await? {
                Some(n) => println!("hid {n} revision(s) with that text"),
                None => bail!("no such revision"),
            }
            Ok(())
        }
        Some("unhide") => {
            let revision = number(rest.first(), "a revision id")?;
            if !moderation_db::unhide(db, revision).await? {
                bail!("no hidden revision {revision}");
            }
            println!("shown again (the bio itself is unchanged)");
            Ok(())
        }
        Some("suspend") => {
            let ids = user_ids(db, &plain(rest, &["--days", "--reason"])).await?;
            let until = match option(rest, "--days") {
                Some(days) => Some(Utc::now() + TimeDelta::days(days.parse().context("--days N")?)),
                None => None,
            };
            let reason = option(rest, "--reason").map_or("", String::as_str);
            let undo = rest.iter().any(|a| a == "--undo");
            let undone = moderation_db::suspend(db, &ids, until, reason, undo).await?;
            println!(
                "suspended {} account(s); bios put back: {}, held edits rejected: {}",
                ids.len(),
                undone.bios,
                undone.rejected
            );
            recount(db).await
        }
        Some("unsuspend") => {
            let ids = user_ids(db, &plain(rest, &[])).await?;
            if !moderation_db::unsuspend(db, &ids).await? {
                println!("none of them was suspended");
            }
            recount(db).await
        }
        Some("untag") => {
            let tag = community::tag_name(rest.first().context("expected a tag")?)?;
            let tag_id = tags_db::tag_id(db, &tag)
                .await?
                .with_context(|| format!("nobody uses `{tag}`"))?;
            let on = [
                ("--artist", CatalogKind::Artist),
                ("--album", CatalogKind::Album),
                ("--track", CatalogKind::Track),
            ]
            .into_iter()
            .find_map(|(flag, kind)| {
                option(rest, flag)
                    .and_then(|id| id.parse().ok())
                    .map(|id| (kind, id))
            });
            let n = moderation_db::untag(db, tag_id, on).await?;
            println!("took `{tag}` off {n} tagging(s)");
            recount(db).await
        }
        Some("block-tag") => {
            let term = community::tag_name(rest.first().context("expected a term")?)?;
            let n = moderation_db::block_term(db, &term).await?;
            println!("blocked `{term}`; took {n} tagging(s) off");
            recount(db).await
        }
        Some("unblock-tag") => {
            let term = community::tag_name(rest.first().context("expected a term")?)?;
            if !moderation_db::unblock_term(db, &term).await? {
                bail!("`{term}` isn't blocked");
            }
            println!("unblocked `{term}`");
            Ok(())
        }
        Some("blocklist") => {
            for term in tags_db::blocklist(db).await? {
                println!("{term}");
            }
            Ok(())
        }
        _ => bail!("unknown moderation command (see `worker --help`)"),
    }
}
