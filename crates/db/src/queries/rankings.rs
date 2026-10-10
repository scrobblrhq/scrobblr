//! Ranking weights and global rankings, shadow mode (see `shared::ranking`
//! for the weights and `migrations/0018_ranking_weights.sql` for the
//! schema).
//!
//! Work is per (user, UTC day), after the day's classification: weighing
//! reads the day's scrobbles with their labels and rewrites the day's rows
//! in `ranking_days` and `ranking_daily`. Days reach `ranking_queue` when
//! their classification is written and from a sweep that compares stored
//! days with the classification, so a lost queue entry or new params are
//! picked up again. Rankings sum `ranking_daily` over a period; the worker
//! keeps each [`Period`]'s in `ranking_snapshots` (migration `0022`).
//! Nothing user-facing reads them yet. Weights are kept only for the days a
//! period can reach ([`retained_from`]).

use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::queries::classification::{QueuedDay, parse_status};
use shared::classification::Status;
use shared::ranking::{self as weights, ClientClass, FULL, Play, RankingParams, Reason};

#[derive(Debug, Clone)]
pub struct Ruleset {
    pub id: i32,
    pub params: RankingParams,
}

/// Plays each reason applied to, indexed like [`Reason::ALL`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReasonCounts(pub [i64; Reason::ALL.len()]);

impl ReasonCounts {
    pub fn get(&self, reason: Reason) -> i64 {
        self.0[reason as usize]
    }

    pub fn add(&mut self, other: &ReasonCounts) {
        for (sum, n) in self.0.iter_mut().zip(other.0) {
            *sum += n;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn from_columns(
        unclassified: i64,
        suspect: i64,
        duplicate: i64,
        no_data: i64,
        imported: i64,
        new_account: i64,
        short_listen: i64,
        no_listened: i64,
        unknown_client: i64,
        track_cap: i64,
        artist_cap: i64,
    ) -> Self {
        Self([
            unclassified,
            suspect,
            duplicate,
            no_data,
            imported,
            new_account,
            short_listen,
            no_listened,
            unknown_client,
            track_cap,
            artist_cap,
        ])
    }
}

/// What weighing a day came to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DayWeights {
    pub plays: i64,
    pub weighted: i64,
    /// Sum of the weights, in thousandths.
    pub weight: i64,
    pub reasons: ReasonCounts,
}

impl DayWeights {
    pub fn add(&mut self, other: &DayWeights) {
        self.plays += other.plays;
        self.weighted += other.weighted;
        self.weight += other.weight;
        self.reasons.add(&other.reasons);
    }
}

fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// Records the ruleset (idempotent) and returns its id.
pub async fn register_ruleset(
    pool: &PgPool,
    params: &RankingParams,
) -> Result<Ruleset, sqlx::Error> {
    let known: Vec<String> = params.known_clients.iter().cloned().collect();
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO ranking_rulesets (fingerprint, params)
        VALUES ($1, jsonb_build_object(
            'weights_version',  $2::int,
            'unknown_client',   $3::bigint / 1000.0,
            'no_listened',      $4::bigint / 1000.0,
            'no_data',          $5::bigint / 1000.0,
            'imported',         $6::bigint / 1000.0,
            'min_account_days', $7::bigint,
            'track_daily_cap',  $8::bigint,
            'artist_daily_cap', $9::bigint,
            'known_clients',    $10::text[]))
        ON CONFLICT (fingerprint) DO UPDATE SET fingerprint = EXCLUDED.fingerprint
        RETURNING id
        "#,
        params.fingerprint(),
        weights::WEIGHTS_VERSION as i32,
        params.unknown_client,
        params.no_listened,
        params.no_data,
        params.imported,
        params.min_account_days,
        params.track_daily_cap,
        params.artist_daily_cap,
        &known,
    )
    .fetch_one(pool)
    .await?;
    Ok(Ruleset {
        id,
        params: params.clone(),
    })
}

/// The stored id of `params`' ruleset, without registering it.
pub async fn find_ruleset(
    pool: &PgPool,
    params: &RankingParams,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT id FROM ranking_rulesets WHERE fingerprint = $1",
        params.fingerprint(),
    )
    .fetch_optional(pool)
    .await
}

/// Queues a day to weigh; classification calls it in the transaction that
/// writes the day.
pub async fn enqueue_day(
    executor: impl PgExecutor<'_>,
    user_id: i64,
    day: NaiveDate,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO ranking_queue (user_id, day) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        user_id,
        day,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Removes up to `limit` queued days and returns them, oldest entries
/// first. A crash before a day is written is repaired by the sweep.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<QueuedDay>, sqlx::Error> {
    sqlx::query_as!(
        QueuedDay,
        r#"
        DELETE FROM ranking_queue
        WHERE (user_id, day) IN (
            SELECT user_id, day FROM ranking_queue
            ORDER BY enqueued_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING user_id, day
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Weighs one user's UTC day with `ruleset` from its stored classification
/// and, unless `dry_run`, replaces its rows. A day without a classification,
/// or before [`retained_from`], has its rows removed. Reads `scrobbles`,
/// never writes it.
pub async fn weigh_user_day(
    pool: &PgPool,
    ruleset: &Ruleset,
    user_id: i64,
    day: NaiveDate,
    dry_run: bool,
) -> Result<DayWeights, sqlx::Error> {
    let start = day_start(day);
    let end = start + TimeDelta::days(1);

    let mut tx = pool.begin().await?;
    // Classification's lock: the labels can't change while they are read.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('classify:' || $1 || ':' || $2, 0))",
    )
    .bind(user_id.to_string())
    .bind(day.to_string())
    .execute(&mut *tx)
    .await?;

    let classified = sqlx::query!(
        r#"
        SELECT d.classified_at, d.max_scrobble_id,
               (u.created_at AT TIME ZONE 'UTC')::date AS "created!"
        FROM scrobble_classification_days d
        JOIN users u ON u.id = d.user_id
        WHERE d.user_id = $1 AND d.day = $2
        "#,
        user_id,
        day,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let retained = dry_run || day >= retained_from(Utc::now().date_naive());
    let Some(classified) = classified.filter(|_| retained) else {
        if !dry_run {
            sqlx::query!(
                "DELETE FROM ranking_days WHERE user_id = $1 AND day = $2",
                user_id,
                day,
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        return Ok(DayWeights::default());
    };

    let rows = sqlx::query!(
        r#"
        SELECT s.id, s.played_at, s.track_id, s.artist_id, s.duration_ms, s.listened_ms,
               s.import_id IS NOT NULL AS "imported!",
               COALESCE(t.duration_ms, t.deezer_duration_ms) AS catalog_duration_ms,
               t.mb_duration_ms,
               c.protocol AS "protocol?", c.name AS "client_name?", c.verified AS "verified?",
               f.status::text AS "flag?"
        FROM scrobbles s
        JOIN tracks t ON t.id = s.track_id
        LEFT JOIN scrobble_clients c ON c.id = s.client_id
        LEFT JOIN scrobble_flags f ON f.scrobble_id = s.id AND f.played_at = s.played_at
        WHERE s.user_id = $1 AND s.played_at >= $2 AND s.played_at < $3
        "#,
        user_id,
        start,
        end,
    )
    .fetch_all(&mut *tx)
    .await?;

    let params = &ruleset.params;
    let plays: Vec<Play> = rows
        .iter()
        .map(|r| Play {
            id: r.id,
            track_id: r.track_id,
            artist_id: r.artist_id,
            played_at: r.played_at,
            status: (r.id <= classified.max_scrobble_id)
                .then(|| r.flag.as_deref().map_or(Status::Counted, parse_status)),
            client: match (&r.protocol, &r.client_name, r.verified) {
                (Some(protocol), Some(name), Some(verified)) => {
                    params.client_class(protocol, name, verified)
                }
                _ => ClientClass::Unknown,
            },
            imported: r.imported,
            listened_ms: r.listened_ms,
            mb_duration_ms: r.mb_duration_ms,
            catalog_duration_ms: r.catalog_duration_ms,
            reported_duration_ms: r.duration_ms,
        })
        .collect();
    let age = (day - classified.created).num_days();
    let weighed = weights::weigh(&plays, age, params);

    let mut outcome = DayWeights {
        plays: plays.len() as i64,
        ..Default::default()
    };
    let by_id: HashMap<i64, &Play> = plays.iter().map(|p| (p.id, p)).collect();
    let mut per_track: HashMap<i64, (i64, i32, i32)> = HashMap::new();
    for w in &weighed {
        let play = by_id[&w.id];
        let entry = per_track
            .entry(play.track_id)
            .or_insert((play.artist_id, 0, 0));
        entry.1 += 1;
        entry.2 += w.permille as i32;
        outcome.weight += w.permille;
        if w.permille > 0 {
            outcome.weighted += 1;
        }
        for reason in w.reasons.iter() {
            outcome.reasons.0[reason as usize] += 1;
        }
    }
    if dry_run {
        tx.rollback().await?;
        return Ok(outcome);
    }

    let r = &outcome.reasons;
    let count = |reason: Reason| r.get(reason) as i32;
    sqlx::query!(
        r#"
        INSERT INTO ranking_days
            (user_id, day, ruleset_id, classified_at, plays, weighted, weight,
             unclassified, suspect, duplicate, no_data, imported, new_account,
             short_listen, no_listened, unknown_client, track_cap, artist_cap)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        ON CONFLICT (user_id, day) DO UPDATE
            SET ruleset_id     = EXCLUDED.ruleset_id,
                classified_at  = EXCLUDED.classified_at,
                plays          = EXCLUDED.plays,
                weighted       = EXCLUDED.weighted,
                weight         = EXCLUDED.weight,
                unclassified   = EXCLUDED.unclassified,
                suspect        = EXCLUDED.suspect,
                duplicate      = EXCLUDED.duplicate,
                no_data        = EXCLUDED.no_data,
                imported       = EXCLUDED.imported,
                new_account    = EXCLUDED.new_account,
                short_listen   = EXCLUDED.short_listen,
                no_listened    = EXCLUDED.no_listened,
                unknown_client = EXCLUDED.unknown_client,
                track_cap      = EXCLUDED.track_cap,
                artist_cap     = EXCLUDED.artist_cap,
                computed_at    = NOW()
        "#,
        user_id,
        day,
        ruleset.id,
        classified.classified_at,
        outcome.plays as i32,
        outcome.weighted as i32,
        outcome.weight,
        count(Reason::Unclassified),
        count(Reason::Suspect),
        count(Reason::Duplicate),
        count(Reason::NoData),
        count(Reason::Imported),
        count(Reason::NewAccount),
        count(Reason::ShortListen),
        count(Reason::NoListened),
        count(Reason::UnknownClient),
        count(Reason::TrackCap),
        count(Reason::ArtistCap),
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "DELETE FROM ranking_daily WHERE user_id = $1 AND day = $2",
        user_id,
        day,
    )
    .execute(&mut *tx)
    .await?;
    let tracks: Vec<i64> = per_track.keys().copied().collect();
    let artists: Vec<i64> = tracks.iter().map(|t| per_track[t].0).collect();
    let counts: Vec<i32> = tracks.iter().map(|t| per_track[t].1).collect();
    let sums: Vec<i32> = tracks.iter().map(|t| per_track[t].2).collect();
    sqlx::query!(
        r#"
        INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
        SELECT $1, $2, t.track_id, t.artist_id, t.plays, t.weight
        FROM UNNEST($3::bigint[], $4::bigint[], $5::int[], $6::int[])
             AS t(track_id, artist_id, plays, weight)
        "#,
        user_id,
        day,
        &tracks,
        &artists,
        &counts,
        &sums,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(outcome)
}

/// Queues days whose weights are missing, made with another ruleset or
/// from an older classification, and days weighed whose classification is
/// gone, from [`retained_from`] on. Newest days first, at most `limit`
/// (`None` = all). Returns the days queued.
pub async fn enqueue_stale(
    conn: &mut PgConnection,
    ruleset_id: i32,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    limit: Option<i64>,
) -> Result<u64, sqlx::Error> {
    let first = retained_from(Utc::now().date_naive());
    let from = Some(from.map_or(first, |from| from.max(first)));
    let queued = sqlx::query!(
        r#"
        WITH classified AS (
            SELECT user_id, day, classified_at FROM scrobble_classification_days
            WHERE ($2::date IS NULL OR day >= $2) AND ($3::date IS NULL OR day <= $3)
        ), weighed AS (
            SELECT user_id, day, ruleset_id, classified_at FROM ranking_days
            WHERE ($2::date IS NULL OR day >= $2) AND ($3::date IS NULL OR day <= $3)
        ), stale AS (
            SELECT user_id, day
            FROM classified c
            FULL JOIN weighed w USING (user_id, day)
            WHERE c.user_id IS NULL
               OR w.user_id IS NULL
               OR w.ruleset_id <> $1
               OR w.classified_at <> c.classified_at
        )
        INSERT INTO ranking_queue (user_id, day)
        SELECT s.user_id, s.day
        FROM stale s
        WHERE NOT EXISTS (
            SELECT 1 FROM ranking_queue q WHERE q.user_id = s.user_id AND q.day = s.day
        )
        ORDER BY s.day DESC
        LIMIT $4
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        ruleset_id,
        from,
        to,
        limit,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(queued)
}

/// Every classified (user, day) in range, or weighed: what a recompute
/// visits.
pub async fn list_days(
    pool: &PgPool,
    user_id: Option<i64>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<Vec<QueuedDay>, sqlx::Error> {
    sqlx::query_as!(
        QueuedDay,
        r#"
        SELECT user_id AS "user_id!", day AS "day!"
        FROM (
            SELECT user_id, day FROM scrobble_classification_days
            UNION
            SELECT user_id, day FROM ranking_days
        ) days
        WHERE ($1::bigint IS NULL OR user_id = $1)
          AND ($2::date IS NULL OR day >= $2)
          AND ($3::date IS NULL OR day <= $3)
        ORDER BY user_id, day
        "#,
        user_id,
        from,
        to,
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
//  Rankings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Artist,
    Track,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Artist, Kind::Track];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Artist => "artist",
            Kind::Track => "track",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One artist or track in a period: raw (every scrobble, every user who
/// scrobbled it) and filtered (weights, listener credits).
#[derive(Debug, Clone)]
pub struct Ranked {
    pub entity_id: i64,
    pub name: String,
    /// The track's artist; `None` for artists.
    pub artist_name: Option<String>,
    pub raw_listeners: i64,
    pub raw_plays: i64,
    /// Sum of listener credits, in thousandths.
    pub listeners: i64,
    /// Sum of weights, in thousandths.
    pub weight: i64,
    /// By listeners, then plays (the rankings' order).
    pub raw_rank: i64,
    pub rank: i64,
    /// By plays alone.
    pub raw_play_rank: i64,
    pub weight_rank: i64,
}

/// Raw and filtered rankings of artists or tracks over the UTC days
/// `[from, to]`, side by side: the entries in the top `limit` of any of the
/// four orders. A user's credit as a listener is their weight for the
/// entity over the period against `listener_weight` (at most one).
pub async fn rankings(
    pool: &PgPool,
    kind: Kind,
    from: NaiveDate,
    to: NaiveDate,
    listener_weight: i64,
    limit: i64,
) -> Result<Vec<Ranked>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        WITH per_user AS (
            SELECT user_id,
                   CASE WHEN $3 THEN track_id ELSE artist_id END AS entity_id,
                   sum(plays)::bigint AS plays,
                   sum(weight)::bigint AS weight
            FROM ranking_daily
            WHERE day BETWEEN $1 AND $2
            GROUP BY 1, 2
        ), per_entity AS (
            SELECT entity_id,
                   count(*) AS raw_listeners,
                   sum(plays)::bigint AS raw_plays,
                   sum(LEAST($5::bigint, weight * $5 / $4))::bigint AS listeners,
                   sum(weight)::bigint AS weight
            FROM per_user
            GROUP BY entity_id
        ), ranked AS (
            SELECT *,
                   row_number() OVER (ORDER BY raw_listeners DESC, raw_plays DESC, entity_id) AS raw_rank,
                   row_number() OVER (ORDER BY listeners DESC, weight DESC, entity_id) AS rank,
                   row_number() OVER (ORDER BY raw_plays DESC, raw_listeners DESC, entity_id) AS raw_play_rank,
                   row_number() OVER (ORDER BY weight DESC, listeners DESC, entity_id) AS weight_rank
            FROM per_entity
        )
        SELECT r.entity_id AS "entity_id!",
               COALESCE(t.title, a.name) AS "name!",
               ta.name AS "artist_name?",
               r.raw_listeners AS "raw_listeners!", r.raw_plays AS "raw_plays!",
               r.listeners AS "listeners!", r.weight AS "weight!",
               r.raw_rank AS "raw_rank!", r.rank AS "rank!",
               r.raw_play_rank AS "raw_play_rank!", r.weight_rank AS "weight_rank!"
        FROM ranked r
        LEFT JOIN tracks t   ON $3 AND t.id = r.entity_id
        LEFT JOIN artists ta ON ta.id = t.artist_id
        LEFT JOIN artists a  ON NOT $3 AND a.id = r.entity_id
        WHERE r.raw_rank <= $6 OR r.rank <= $6 OR r.raw_play_rank <= $6 OR r.weight_rank <= $6
        ORDER BY r.rank
        "#,
        from,
        to,
        kind == Kind::Track,
        listener_weight.max(1),
        FULL,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Ranked {
            entity_id: r.entity_id,
            name: r.name,
            artist_name: r.artist_name,
            raw_listeners: r.raw_listeners,
            raw_plays: r.raw_plays,
            listeners: r.listeners,
            weight: r.weight,
            raw_rank: r.raw_rank,
            rank: r.rank,
            raw_play_rank: r.raw_play_rank,
            weight_rank: r.weight_rank,
        })
        .collect())
}

// ---------------------------------------------------------------------------
//  Snapshots: each period's ranking, precomputed
// ---------------------------------------------------------------------------

/// A rolling window of UTC days ending today, today included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Week,
    Month,
    Year,
}

impl Period {
    pub const ALL: [Period; 3] = [Period::Week, Period::Month, Period::Year];

    pub fn as_str(self) -> &'static str {
        match self {
            Period::Week => "week",
            Period::Month => "month",
            Period::Year => "year",
        }
    }

    pub fn parse(s: &str) -> Option<Period> {
        Period::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub const fn days(self) -> i64 {
        match self {
            Period::Week => 7,
            Period::Month => 30,
            Period::Year => 365,
        }
    }

    /// The first and last day of the period ending `today`.
    pub fn range(self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        (today - TimeDelta::days(self.days() - 1), today)
    }
}

/// Days of weights kept: the longest period, and a week more so a snapshot
/// computed around midnight, or by a lagging clock, finds its first day.
pub const RETAINED_DAYS: i64 = Period::Year.days() + 7;

/// The first day whose weights are kept on `today`.
pub fn retained_from(today: NaiveDate) -> NaiveDate {
    today - TimeDelta::days(RETAINED_DAYS - 1)
}

/// Deletes the weights of days before [`retained_from`]`(today)`; returns
/// the user-days removed. No period reaches them.
pub async fn purge_expired(pool: &PgPool, today: NaiveDate) -> Result<u64, sqlx::Error> {
    let first = retained_from(today);
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM ranking_daily WHERE day < $1", first)
        .execute(&mut *tx)
        .await?;
    let days = sqlx::query!("DELETE FROM ranking_days WHERE day < $1", first)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(days)
}

/// A period's stored ranking of artists or tracks.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub period: Period,
    pub kind: Kind,
    pub from_day: NaiveDate,
    pub to_day: NaiveDate,
    pub ruleset_id: i32,
    /// Entities with any weight; the snapshot keeps the first of them.
    pub ranked: i32,
    /// User-days of the period still queued for weighing when it ran.
    pub pending_days: i32,
    pub computed_at: DateTime<Utc>,
    pub took_ms: i32,
}

/// Ranks `period`'s artists and tracks as [`rankings`] orders them
/// filtered, leaving out entities without weight, and replaces the
/// period's snapshots with the first `top` of each in one transaction.
pub async fn refresh_snapshots(
    pool: &PgPool,
    ruleset: &Ruleset,
    period: Period,
    today: NaiveDate,
    top: i64,
) -> Result<Vec<Snapshot>, sqlx::Error> {
    let (from, to) = period.range(today);
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10min'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('rankings:' || $1, 0))")
        .bind(period.as_str())
        .execute(&mut *tx)
        .await?;
    let pending_days = sqlx::query_scalar!(
        r#"SELECT count(*)::int AS "n!" FROM ranking_queue WHERE day BETWEEN $1 AND $2"#,
        from,
        to,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM ranking_snapshots WHERE period = $1",
        period.as_str()
    )
    .execute(&mut *tx)
    .await?;

    let mut snapshots = Vec::new();
    for kind in Kind::ALL {
        let started = Instant::now();
        let ranked = sqlx::query_scalar!(
            r#"
            WITH per_user AS (
                SELECT user_id,
                       CASE WHEN $3 THEN track_id ELSE artist_id END AS entity_id,
                       sum(plays)::bigint AS plays,
                       sum(weight)::bigint AS weight
                FROM ranking_daily
                WHERE day BETWEEN $1 AND $2
                GROUP BY 1, 2
            ), per_entity AS (
                SELECT entity_id,
                       count(*)::int AS raw_listeners,
                       sum(plays)::int AS raw_plays,
                       sum(LEAST($5::bigint, weight * $5 / $4))::bigint AS listeners,
                       sum(weight)::bigint AS weight
                FROM per_user
                GROUP BY entity_id
                HAVING sum(weight) > 0
            ), top AS (
                SELECT * FROM per_entity
                ORDER BY listeners DESC, weight DESC, entity_id
                LIMIT $8
            ), inserted AS (
                INSERT INTO ranking_entries
                    (period, kind, position, entity_id, listeners, weight, raw_listeners, raw_plays)
                SELECT $6, $7,
                       row_number() OVER (ORDER BY listeners DESC, weight DESC, entity_id),
                       entity_id, listeners, weight, raw_listeners, raw_plays
                FROM top
            )
            SELECT count(*)::int AS "ranked!" FROM per_entity
            "#,
            from,
            to,
            kind == Kind::Track,
            ruleset.params.listener_weight().max(1),
            FULL,
            period.as_str(),
            kind.as_str(),
            top,
        )
        .fetch_one(&mut *tx)
        .await?;
        let took_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        let computed_at = sqlx::query_scalar!(
            r#"
            INSERT INTO ranking_snapshots
                (period, kind, from_day, to_day, ruleset_id, ranked, pending_days, took_ms)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING computed_at
            "#,
            period.as_str(),
            kind.as_str(),
            from,
            to,
            ruleset.id,
            ranked,
            pending_days,
            took_ms,
        )
        .fetch_one(&mut *tx)
        .await?;
        snapshots.push(Snapshot {
            period,
            kind,
            from_day: from,
            to_day: to,
            ruleset_id: ruleset.id,
            ranked,
            pending_days,
            computed_at,
            took_ms,
        });
    }
    tx.commit().await?;
    Ok(snapshots)
}

pub async fn snapshots(pool: &PgPool) -> Result<Vec<Snapshot>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT period, kind, from_day, to_day, ruleset_id, ranked, pending_days,
               computed_at, took_ms
        FROM ranking_snapshots
        ORDER BY period, kind
        "#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(Snapshot {
                period: Period::parse(&r.period)?,
                kind: Kind::parse(&r.kind)?,
                from_day: r.from_day,
                to_day: r.to_day,
                ruleset_id: r.ruleset_id,
                ranked: r.ranked,
                pending_days: r.pending_days,
                computed_at: r.computed_at,
                took_ms: r.took_ms,
            })
        })
        .collect())
}

/// One position of a stored ranking.
#[derive(Debug, Clone)]
pub struct Entry {
    pub position: i32,
    pub entity_id: i64,
    /// `None` when the entity was deleted since.
    pub name: Option<String>,
    /// The track's artist; `None` for artists.
    pub artist_name: Option<String>,
    /// Thousandths.
    pub listeners: i64,
    pub weight: i64,
    pub raw_listeners: i32,
    pub raw_plays: i32,
}

/// Positions `offset + 1 ..= offset + limit` of a stored ranking, what a
/// route would serve.
pub async fn snapshot_entries(
    pool: &PgPool,
    period: Period,
    kind: Kind,
    offset: i64,
    limit: i64,
) -> Result<Vec<Entry>, sqlx::Error> {
    sqlx::query_as!(
        Entry,
        r#"
        SELECT e.position, e.entity_id,
               COALESCE(t.title, a.name) AS "name?",
               ta.name AS "artist_name?",
               e.listeners, e.weight, e.raw_listeners, e.raw_plays
        FROM ranking_entries e
        LEFT JOIN tracks t   ON e.kind = 'track' AND t.id = e.entity_id
        LEFT JOIN artists ta ON ta.id = t.artist_id
        LEFT JOIN artists a  ON e.kind = 'artist' AND a.id = e.entity_id
        WHERE e.period = $1 AND e.kind = $2 AND e.position > $3
        ORDER BY e.position
        LIMIT $4
        "#,
        period.as_str(),
        kind.as_str(),
        offset as i32,
        limit,
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
//  Reporting (worker CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Coverage {
    pub classified_days: i64,
    pub weighed_days: i64,
    /// Weighed with the current ruleset from the current classification.
    pub current: i64,
    pub queued: i64,
}

pub async fn coverage(
    pool: &PgPool,
    ruleset_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Coverage, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT count(*) FROM scrobble_classification_days WHERE day BETWEEN $2 AND $3) AS "classified!",
            (SELECT count(*) FROM ranking_days WHERE day BETWEEN $2 AND $3) AS "weighed!",
            (SELECT count(*) FROM ranking_days r
             JOIN scrobble_classification_days d USING (user_id, day)
             WHERE r.day BETWEEN $2 AND $3 AND r.ruleset_id = $1
               AND r.classified_at = d.classified_at) AS "current!",
            (SELECT count(*) FROM ranking_queue WHERE day BETWEEN $2 AND $3) AS "queued!"
        "#,
        ruleset_id,
        from,
        to,
    )
    .fetch_one(pool)
    .await?;
    Ok(Coverage {
        classified_days: row.classified,
        weighed_days: row.weighed,
        current: row.current,
        queued: row.queued,
    })
}

/// Totals over the range, all users together.
pub async fn totals(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<DayWeights, sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT COALESCE(sum(plays), 0)::bigint AS "plays!", COALESCE(sum(weighted), 0)::bigint AS "weighted!",
               COALESCE(sum(weight), 0)::bigint AS "weight!",
               COALESCE(sum(unclassified), 0)::bigint AS "unclassified!", COALESCE(sum(suspect), 0)::bigint AS "suspect!",
               COALESCE(sum(duplicate), 0)::bigint AS "duplicate!", COALESCE(sum(no_data), 0)::bigint AS "no_data!",
               COALESCE(sum(imported), 0)::bigint AS "imported!", COALESCE(sum(new_account), 0)::bigint AS "new_account!",
               COALESCE(sum(short_listen), 0)::bigint AS "short_listen!", COALESCE(sum(no_listened), 0)::bigint AS "no_listened!",
               COALESCE(sum(unknown_client), 0)::bigint AS "unknown_client!", COALESCE(sum(track_cap), 0)::bigint AS "track_cap!",
               COALESCE(sum(artist_cap), 0)::bigint AS "artist_cap!"
        FROM ranking_days
        WHERE day BETWEEN $1 AND $2
        "#,
        from,
        to,
    )
    .fetch_one(pool)
    .await?;
    Ok(DayWeights {
        plays: r.plays,
        weighted: r.weighted,
        weight: r.weight,
        reasons: ReasonCounts::from_columns(
            r.unclassified,
            r.suspect,
            r.duplicate,
            r.no_data,
            r.imported,
            r.new_account,
            r.short_listen,
            r.no_listened,
            r.unknown_client,
            r.track_cap,
            r.artist_cap,
        ),
    })
}

/// One user's share of the range.
#[derive(Debug)]
pub struct Contributor {
    pub user_id: i64,
    pub username: String,
    pub totals: DayWeights,
    pub raw_rank: i64,
    pub rank: i64,
}

/// Users ranked by their plays (raw) and by their weight (filtered) over
/// the range: the entries in the top `limit` of either, and how many
/// contributed at all.
pub async fn contributors(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    limit: i64,
) -> Result<(i64, Vec<Contributor>), sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        WITH per_user AS (
            SELECT user_id,
                   sum(plays)::bigint AS plays, sum(weighted)::bigint AS weighted,
                   sum(weight)::bigint AS weight,
                   sum(unclassified)::bigint AS unclassified, sum(suspect)::bigint AS suspect,
                   sum(duplicate)::bigint AS duplicate, sum(no_data)::bigint AS no_data,
                   sum(imported)::bigint AS imported, sum(new_account)::bigint AS new_account,
                   sum(short_listen)::bigint AS short_listen, sum(no_listened)::bigint AS no_listened,
                   sum(unknown_client)::bigint AS unknown_client, sum(track_cap)::bigint AS track_cap,
                   sum(artist_cap)::bigint AS artist_cap
            FROM ranking_days
            WHERE day BETWEEN $1 AND $2
            GROUP BY user_id
        ), ranked AS (
            SELECT *,
                   row_number() OVER (ORDER BY plays DESC, user_id) AS raw_rank,
                   row_number() OVER (ORDER BY weight DESC, user_id) AS rank,
                   count(*) OVER () AS users
            FROM per_user
        )
        SELECT r.user_id AS "user_id!", u.username, r.users AS "users!",
               r.plays AS "plays!", r.weighted AS "weighted!", r.weight AS "weight!",
               r.unclassified AS "unclassified!", r.suspect AS "suspect!",
               r.duplicate AS "duplicate!", r.no_data AS "no_data!",
               r.imported AS "imported!", r.new_account AS "new_account!",
               r.short_listen AS "short_listen!", r.no_listened AS "no_listened!",
               r.unknown_client AS "unknown_client!", r.track_cap AS "track_cap!",
               r.artist_cap AS "artist_cap!",
               r.raw_rank AS "raw_rank!", r.rank AS "rank!"
        FROM ranked r
        JOIN users u ON u.id = r.user_id
        WHERE r.raw_rank <= $3 OR r.rank <= $3
        ORDER BY r.raw_rank
        "#,
        from,
        to,
        limit,
    )
    .fetch_all(pool)
    .await?;
    let users = rows.first().map_or(0, |r| r.users);
    Ok((
        users,
        rows.into_iter()
            .map(|r| Contributor {
                user_id: r.user_id,
                username: r.username,
                totals: DayWeights {
                    plays: r.plays,
                    weighted: r.weighted,
                    weight: r.weight,
                    reasons: ReasonCounts::from_columns(
                        r.unclassified,
                        r.suspect,
                        r.duplicate,
                        r.no_data,
                        r.imported,
                        r.new_account,
                        r.short_listen,
                        r.no_listened,
                        r.unknown_client,
                        r.track_cap,
                        r.artist_cap,
                    ),
                },
                raw_rank: r.raw_rank,
                rank: r.rank,
            })
            .collect(),
    ))
}

/// The first and last weighed days, for reports over all history.
pub async fn weighed_range(pool: &PgPool) -> Result<Option<(NaiveDate, NaiveDate)>, sqlx::Error> {
    let row = sqlx::query!(r#"SELECT min(day) AS "from", max(day) AS "to" FROM ranking_days"#)
        .fetch_one(pool)
        .await?;
    Ok(row.from.zip(row.to))
}
