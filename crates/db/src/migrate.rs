//! Migration runner for the numbered files in `migrations/`.
//!
//! Files are embedded at compile time and applied in version order, each one
//! recorded in `schema_migrations` together with its checksum. A file whose
//! first line is `-- no-transaction` runs one statement at a time outside a
//! transaction (Timescale refuses `refresh_continuous_aggregate` inside one,
//! and a multi-statement query is an implicit transaction); such files must be
//! safe to re-run, since a failure halfway leaves earlier statements applied.
//!
//! Runs hold a session advisory lock, so concurrent invocations serialize and
//! the later one finds nothing left to do.

use std::collections::HashMap;
use std::time::Instant;

use sqlx::migrate::{Migration, Migrator};
use sqlx::{ConnectOptions, Connection, PgConnection, PgPool};
use thiserror::Error;

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

const LOCK_KEY: i64 = 0x5343_524f_424c_5201; // "SCROBLR" + 1

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration {version} ({description}) failed: {source}")]
    Failed {
        version: i64,
        description: String,
        source: sqlx::Error,
    },
    #[error("migration {0} was modified after it was applied")]
    ChecksumMismatch(i64),
    #[error(
        "migration {pending} is older than the newest applied migration {applied}; renumber it"
    )]
    OutOfOrder { pending: i64, applied: i64 },
    #[error(
        "database is not migrated: {0} pending migration(s), starting with {1}. Run `just migrate`"
    )]
    Pending(usize, String),
}

/// One migration as seen by [`status`].
#[derive(Debug)]
pub struct MigrationStatus {
    pub version: i64,
    pub description: String,
    pub applied: bool,
}

/// Applies every pending migration in order and returns the versions applied.
pub async fn run(pool: &PgPool) -> Result<Vec<i64>, MigrateError> {
    apply(pool, None).await
}

/// Records every migration up to and including `version` as applied without
/// running it, for databases whose schema was created by hand before the
/// runner existed. Later migrations are then applied normally.
pub async fn baseline(pool: &PgPool, version: i64) -> Result<Vec<i64>, MigrateError> {
    apply(pool, Some(version)).await
}

/// Fails unless every embedded migration has been applied unmodified. The API
/// and the worker call this at startup instead of migrating themselves.
pub async fn ensure_current(pool: &PgPool) -> Result<(), MigrateError> {
    let mut conn = pool.acquire().await?;
    let applied = applied(&mut conn).await?;
    let pending = check(&applied)?;
    match pending.first() {
        None => Ok(()),
        Some(first) => Err(MigrateError::Pending(pending.len(), label(first))),
    }
}

/// Every embedded migration and whether the database has applied it.
pub async fn status(pool: &PgPool) -> Result<Vec<MigrationStatus>, MigrateError> {
    let mut conn = pool.acquire().await?;
    let applied = applied(&mut conn).await?;
    Ok(MIGRATOR
        .iter()
        .map(|m| MigrationStatus {
            version: m.version,
            description: m.description.to_string(),
            applied: applied.contains_key(&m.version),
        })
        .collect())
}

async fn apply(pool: &PgPool, baseline: Option<i64>) -> Result<Vec<i64>, MigrateError> {
    // A dedicated connection, closed on drop, so the session lock can never
    // leak back into the pool on an early return.
    // Migrations are slow by nature, and a slow one would be logged whole.
    let options = (*pool.connect_options())
        .clone()
        .disable_statement_logging();
    let mut conn = PgConnection::connect_with(&options).await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut conn)
        .await?;

    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version      BIGINT       PRIMARY KEY,
            description  TEXT         NOT NULL,
            checksum     BYTEA        NOT NULL,
            applied_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
            execution_ms BIGINT       NOT NULL
        )
        "#,
    )
    .execute(&mut conn)
    .await?;

    let applied = applied(&mut conn).await?;
    let mut done = Vec::new();
    for migration in check(&applied)? {
        let started = Instant::now();
        if baseline.is_some_and(|b| migration.version <= b) {
            record(&mut conn, migration, 0).await?;
            tracing::info!("migrate: baselined {}", label(migration));
        } else {
            execute(&mut conn, migration, started)
                .await
                .map_err(|source| MigrateError::Failed {
                    version: migration.version,
                    description: migration.description.to_string(),
                    source,
                })?;
            tracing::info!(
                "migrate: applied {} in {} ms",
                label(migration),
                started.elapsed().as_millis()
            );
        }
        done.push(migration.version);
    }

    conn.close().await?;
    Ok(done)
}

async fn execute(
    conn: &mut PgConnection,
    migration: &Migration,
    started: Instant,
) -> Result<(), sqlx::Error> {
    if migration.no_tx {
        for statement in split_statements(&migration.sql) {
            execute_retrying(conn, statement).await?;
        }
        record(&mut *conn, migration, elapsed_ms(started)).await
    } else {
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(&migration.sql).execute(&mut *tx).await?;
        record(&mut tx, migration, elapsed_ms(started)).await?;
        tx.commit().await
    }
}

/// Runs one statement of a no-transaction migration, retrying while a lock
/// is busy: Timescale's scheduler can start a policy refresh of the same
/// aggregate the migration refreshes, which fails with `lock_not_available`.
async fn execute_retrying(conn: &mut PgConnection, statement: &str) -> Result<(), sqlx::Error> {
    const ATTEMPTS: u32 = 10;
    let mut attempt = 1;
    loop {
        match sqlx::raw_sql(statement).execute(&mut *conn).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt < ATTEMPTS && is_lock_not_available(&e) => {
                tracing::warn!("migrate: lock busy, retrying ({attempt}/{ATTEMPTS}): {e}");
                tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                    .await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_lock_not_available(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "55P03")
}

async fn record(
    conn: &mut PgConnection,
    migration: &Migration,
    execution_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO schema_migrations (version, description, checksum, execution_ms) VALUES ($1, $2, $3, $4)",
    )
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(&*migration.checksum)
    .bind(execution_ms)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn applied(conn: &mut PgConnection) -> Result<HashMap<i64, Vec<u8>>, sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('schema_migrations') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM schema_migrations")
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Validates applied migrations against the embedded ones and returns the
/// pending ones in order. Versions applied by a newer binary are tolerated.
fn check(applied: &HashMap<i64, Vec<u8>>) -> Result<Vec<&'static Migration>, MigrateError> {
    let newest_applied = applied.keys().max().copied();
    let mut pending = Vec::new();
    for migration in MIGRATOR.iter() {
        match applied.get(&migration.version) {
            Some(checksum) if *checksum != *migration.checksum => {
                return Err(MigrateError::ChecksumMismatch(migration.version));
            }
            Some(_) => {}
            None => {
                if let Some(newest) = newest_applied.filter(|n| *n > migration.version) {
                    return Err(MigrateError::OutOfOrder {
                        pending: migration.version,
                        applied: newest,
                    });
                }
                pending.push(migration);
            }
        }
    }
    Ok(pending)
}

fn label(migration: &Migration) -> String {
    format!("{:04} {}", migration.version, migration.description)
}

fn elapsed_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Splits a SQL script into top-level statements, respecting quotes,
/// dollar-quoted bodies and comments. Comment-only fragments are dropped.
pub fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = sql[i..].find('\n').map_or(bytes.len(), |n| i + n + 1);
                if !has_code {
                    start = i;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < bytes.len() {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if !has_code {
                    start = i;
                }
                continue;
            }
            quote @ (b'\'' | b'"') => {
                let escapes = quote == b'\''
                    && i > 0
                    && matches!(bytes[i - 1], b'E' | b'e')
                    && (i < 2 || !bytes[i - 2].is_ascii_alphanumeric() && bytes[i - 2] != b'_');
                i += 1;
                while i < bytes.len() {
                    if escapes && bytes[i] == b'\\' {
                        i += 2;
                    } else if bytes[i] == quote {
                        // A doubled quote is an escaped quote, not the end.
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                i += 1;
                has_code = true;
                continue;
            }
            b'$' => {
                if let Some(tag) = dollar_tag(&sql[i..]) {
                    let body = i + tag.len();
                    i = sql[body..]
                        .find(tag)
                        .map_or(bytes.len(), |n| body + n + tag.len());
                    has_code = true;
                    continue;
                }
                has_code = true;
            }
            b';' => {
                if has_code {
                    statements.push(sql[start..i].trim());
                }
                start = i + 1;
                has_code = false;
            }
            c if !c.is_ascii_whitespace() => has_code = true,
            _ => {}
        }
        i += 1;
    }

    if has_code {
        statements.push(sql[start..].trim());
    }
    statements
}

/// `$tag$` or `$$` at the start of `s`. A `$` followed by a digit is a
/// positional parameter, not a dollar quote.
fn dollar_tag(s: &str) -> Option<&str> {
    let rest = &s[1..];
    let end = rest.find('$')?;
    let tag = &rest[..end];
    let valid = tag.is_empty()
        || (!tag.starts_with(|c: char| c.is_ascii_digit())
            && tag.chars().all(|c| c.is_alphanumeric() || c == '_'));
    valid.then(|| &s[..end + 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_top_level_statements() {
        let sql =
            "SELECT 1; -- trailing; comment\nSELECT 'a;b'; /* x; /* nested; */ y */ SELECT 2;";
        assert_eq!(
            split_statements(sql),
            ["SELECT 1", "SELECT 'a;b'", "SELECT 2"]
        );
    }

    #[test]
    fn keeps_dollar_quoted_bodies_whole() {
        let sql = "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $fn$ SELECT 1; $fn$;\nDO $$ BEGIN PERFORM 1; END $$;";
        let statements = split_statements(sql);
        assert_eq!(statements.len(), 2);
        assert!(statements[0].ends_with("$fn$"));
        assert!(statements[1].starts_with("DO $$"));
    }

    #[test]
    fn handles_escaped_quotes_and_identifiers() {
        let sql = r#"SELECT 'it''s; fine', E'back\'slash;', "odd;name" FROM t; SELECT $1"#;
        assert_eq!(split_statements(sql).len(), 2);
    }

    #[test]
    fn drops_comment_only_fragments() {
        assert_eq!(
            split_statements("-- only a comment\n;\n  ;  /* c */"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn embedded_migrations_are_ordered_and_unique() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert_eq!(versions.first(), Some(&1));
    }

    #[test]
    fn aggregate_backfill_runs_outside_a_transaction() {
        let m = MIGRATOR.iter().find(|m| m.version == 8).unwrap();
        assert!(m.no_tx);
        assert_eq!(
            split_statements(&m.sql)
                .iter()
                .filter(|s| s.starts_with("CALL"))
                .count(),
            3
        );
    }
}
