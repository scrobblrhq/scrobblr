//! `worker uploads gc`: finds the files under `UPLOAD_DIR` no row refers to
//! any more (avatars from before upload keys once replaced, deletes the API
//! couldn't finish, uploads whose database write never happened) and deletes
//! them with `--delete`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use sqlx::PgPool;

use db::queries::uploads as uploads_db;
use shared::media::UploadKey;

pub const USAGE: &str = "       worker uploads gc [--delete] [--min-age-hours N]
                                  list the files under UPLOAD_DIR no row refers to;
                                  --delete deletes them. Files younger than N hours
                                  (default 24) are left alone: their upload may not
                                  have reached the database yet";

const DEFAULT_MIN_AGE: Duration = Duration::from_secs(24 * 3600);

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    if args.first().map(String::as_str) != Some("gc") {
        bail!("unknown uploads command (see `worker --help`)");
    }
    let mut delete = false;
    let mut min_age = DEFAULT_MIN_AGE;
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--delete" => delete = true,
            "--min-age-hours" => {
                let hours: u64 = args
                    .next()
                    .context("--min-age-hours needs a number")?
                    .parse()
                    .context("--min-age-hours needs a whole number of hours")?;
                min_age = Duration::from_secs(hours * 3600);
            }
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }
    let root =
        PathBuf::from(crate::non_empty_env("UPLOAD_DIR").unwrap_or_else(|| "uploads".into()));
    if !root.is_dir() {
        bail!("UPLOAD_DIR {} is not a directory", root.display());
    }

    let report = gc(db, &root, min_age, delete).await?;
    for key in &report.orphans {
        println!("{key}");
    }
    let verb = if delete {
        "deleted"
    } else {
        "unreferenced (run with --delete to delete)"
    };
    println!(
        "{} upload(s) in {}: {} referenced, {} {verb}, {:.1} MB; {} unreferenced but younger than {} h; {} other file(s) left alone",
        report.scanned,
        root.display(),
        report.referenced,
        report.orphans.len() - report.failed,
        report.orphan_bytes as f64 / 1_000_000.0,
        report.young,
        min_age.as_secs() / 3600,
        report.other,
    );
    if report.failed > 0 {
        bail!(
            "{} file(s) could not be deleted (see the log)",
            report.failed
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Report {
    /// Files whose path is an upload key.
    pub scanned: usize,
    pub referenced: usize,
    pub orphans: Vec<String>,
    pub orphan_bytes: u64,
    pub young: usize,
    /// Files that aren't uploads, never touched.
    pub other: usize,
    pub failed: usize,
}

struct Stored {
    key: String,
    bytes: u64,
    modified: SystemTime,
}

/// Lists the files before reading the references: an upload stored in
/// between is younger than `min_age`, so it is never taken for an orphan.
pub async fn gc(
    db: &PgPool,
    root: &Path,
    min_age: Duration,
    delete: bool,
) -> anyhow::Result<Report> {
    let scan_root = root.to_path_buf();
    let (files, other) = tokio::task::spawn_blocking(move || scan(&scan_root))
        .await?
        .with_context(|| format!("could not read {}", root.display()))?;
    let referenced: HashSet<String> = uploads_db::referenced_keys(db).await?.into_iter().collect();

    let now = SystemTime::now();
    let mut report = Report {
        scanned: files.len(),
        other,
        ..Default::default()
    };
    for file in files {
        if referenced.contains(&file.key) {
            report.referenced += 1;
        } else if now.duration_since(file.modified).unwrap_or_default() < min_age {
            report.young += 1;
        } else {
            report.orphan_bytes += file.bytes;
            report.orphans.push(file.key);
        }
    }
    if delete {
        let root = root.to_path_buf();
        let orphans = report.orphans.clone();
        report.failed = tokio::task::spawn_blocking(move || remove(&root, &orphans)).await?;
    }
    Ok(report)
}

/// Every file under `root` whose path is an upload key, and how many others
/// there are. Hidden entries (`.tmp/`, where the API writes) are skipped.
fn scan(root: &Path) -> io::Result<(Vec<Stored>, usize)> {
    let mut files = Vec::new();
    let mut other = 0;
    let mut dirs = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                other += 1;
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let relative = format!("{prefix}{name}");
            let meta = entry.metadata()?;
            if meta.is_dir() {
                dirs.push((entry.path(), format!("{relative}/")));
            } else if meta.is_file() && UploadKey::parse(&relative).is_some() {
                files.push(Stored {
                    key: relative,
                    bytes: meta.len(),
                    modified: meta.modified()?,
                });
            } else {
                other += 1;
            }
        }
    }
    Ok((files, other))
}

/// Deletes `keys` and the shard directories they leave empty; returns how
/// many couldn't be deleted.
fn remove(root: &Path, keys: &[String]) -> usize {
    let mut failed = 0;
    for key in keys {
        let path = root.join(key);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(%key, "could not delete upload: {e}");
                failed += 1;
                continue;
            }
        }
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| *d != root) {
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_db;

    fn write(root: &Path, key: &str, age: Duration) {
        let path = root.join(key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"jpeg").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn deletes_only_old_files_no_row_refers_to() {
        with_db(|pool| async move {
            let root =
                std::env::temp_dir().join(format!("scrobblr_gc_{}", uuid::Uuid::new_v4().simple()));
            let old = Duration::from_secs(48 * 3600);
            let avatar = "avatars/aa/aa000000000000000000000000000000.jpg";
            let candidate = "artists/bb/bb000000000000000000000000000000.jpg";
            let legacy = "0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg";
            let orphan = "avatars/cc/cc000000000000000000000000000000.jpg";
            let legacy_orphan = "1b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg";
            let young = "albums/dd/dd000000000000000000000000000000.jpg";
            for key in [avatar, candidate, legacy, orphan, legacy_orphan] {
                write(&root, key, old);
            }
            write(&root, young, Duration::ZERO);
            write(&root, ".tmp/x.part", old);
            write(&root, "notes.txt", old);

            let user: i64 = sqlx::query_scalar(
                "INSERT INTO users (username, email, password_hash, image_url) \
                 VALUES ('gc', 'gc@test', 'x', $1) RETURNING id",
            )
            .bind(avatar)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO artists (name, name_normalized, image_url) VALUES ('a', 'a', $1)",
            )
            .bind(legacy)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO image_candidates (entity_type, entity_id, url, uploaded_by) \
                 VALUES ('artist', 1, $1, $2)",
            )
            .bind(candidate)
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();

            let dry = gc(&pool, &root, DEFAULT_MIN_AGE, false).await.unwrap();
            assert_eq!(dry.scanned, 6);
            assert_eq!(dry.referenced, 3);
            assert_eq!(dry.young, 1);
            assert_eq!(dry.other, 1);
            let mut orphans = dry.orphans.clone();
            orphans.sort();
            assert_eq!(orphans, [legacy_orphan, orphan]);
            assert!(root.join(orphan).exists());

            let done = gc(&pool, &root, DEFAULT_MIN_AGE, true).await.unwrap();
            assert_eq!(done.failed, 0);
            for key in [orphan, legacy_orphan] {
                assert!(!root.join(key).exists(), "{key}");
            }
            assert!(!root.join("avatars/cc").exists());
            assert!(root.join("avatars/aa").exists());
            for key in [avatar, candidate, legacy, young, ".tmp/x.part", "notes.txt"] {
                assert!(root.join(key).exists(), "{key}");
            }
            fs::remove_dir_all(root).unwrap();
        })
        .await;
    }
}
