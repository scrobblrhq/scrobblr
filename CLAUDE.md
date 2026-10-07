# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Scrobblr — a music scrobbling service (self-hosted last.fm alternative). This is the **backend** repo: a Rust workspace (Axum + SQLx + PostgreSQL/TimescaleDB + Redis) plus the `@scrobblr/types` package (TypeScript types generated from the Rust models via ts-rs, published to npm). The browser extension and Flutter app live in sibling repos (`scrobblrhq/extension`, `scrobblrhq/mobile`) and consume `@scrobblr/types` from npm.

## Commands

Dev environment is managed with devenv (nix): `devenv up` starts PostgreSQL (with TimescaleDB, DB `scrobblr`) and Redis (password `123`), then runs the migration runner once Postgres is ready. `.env` is loaded automatically (dotenv is enabled in devenv and via `dotenvy` at runtime).

```bash
cargo run -p api          # run the API (requires DATABASE_URL; listens on BIND_ADDR, default 0.0.0.0:8080)
cargo run -p worker       # background jobs (session/now_playing cleanup, metadata enrichment, now-playing SSE republish, scrobble classification)
cargo run -p worker -- classify report   # classification review CLI (`worker --help` lists commands)
just migrate              # apply pending migrations (`just migrate status` lists them)
just import-lastfm USER LASTFM_USER   # import a Last.fm history in the foreground (needs LASTFM_API_KEY)
just import-status        # progress of recent imports

just fmt                  # cargo fmt --all
just lint                 # cargo clippy --workspace --all-targets -- -D warnings
just lint-fix             # clippy --fix
just check                # cargo check --workspace
just ci                   # fmt-check + lint + check + test + build

just test                 # cargo test --workspace; also regenerates TS bindings (see Types pipeline below)
just test-db              # #[ignore]d database tests (need Postgres; see Migrations)
cargo test -p api <name>  # single test

# JS side (bun is the package manager; biome for lint/format)
bun install
turbo run build           # per-package build tasks (crates have package.json wrappers)
cd apps/extension && bun run dev   # Plasmo extension dev mode

# Mobile app (Flutter + Android SDK provided by devenv's android module)
cd apps/mobile && flutter pub get && flutter test   # pipeline unit tests
cd apps/mobile && flutter run                        # Android emulator (server: http://10.0.2.2:8080)
```

### SQLx offline mode

Query macros (`sqlx::query!` etc.) compile against the `.sqlx/` cache, so no database is needed to build (but with `DATABASE_URL` set, as `.env` does, they check against the live database instead, which must then be migrated; `SQLX_OFFLINE=true` forces the cache). When you add or change a query, you need a live `DATABASE_URL` and must run `cargo sqlx prepare --workspace` (sqlx-cli is in the devenv shell) and commit the updated `.sqlx/` files.

### Migrations

Numbered plain-SQL files in `migrations/` (`0001_initial.sql` … next free number), applied **in order** by `just migrate` (= `SQLX_OFFLINE=true cargo run -p worker -- migrate`; offline because it must compile before the database has the schema the query macros expect). The runner is `crates/db/src/migrate.rs`: files are embedded at compile time via `sqlx::migrate!` (`crates/db/build.rs` rebuilds on changes), each applied one is recorded with its checksum in `schema_migrations`, and a session advisory lock serializes concurrent runs. Each file runs in one transaction unless its **first line** is `-- no-transaction` (e.g. `0008`, whose `CALL refresh_continuous_aggregate` can't run in one): then each statement runs on its own (retried while it hits `lock_not_available`, e.g. a policy refresh the scheduler just started), so such a file must be safe to re-run. Never edit an applied migration (checksum mismatch) — add a new one. Don't use `sqlx migrate run`: sqlx-cli sends a no-transaction file as one multi-statement query, which Postgres wraps in an implicit transaction.

Migration is a deploy step, never automatic: the API and worker only call `db::migrate::ensure_current` at startup and refuse to run with pending migrations. devenv runs the runner after Postgres starts; docker-compose has a one-shot `migrate` service the API and worker depend on. `just migrate --baseline N` records `1..=N` as applied without running them (for databases migrated by hand before the runner existed). DB tests (`crates/db/tests/`, `#[ignore]`d) each create a throwaway database from `DATABASE_URL`, migrate it through the runner, and drop it.

(The README's mention of a `crates/core` crate is stale — the actual crate is `crates/shared`.)

## Architecture

Rust workspace crates and their dependency direction: `api` → `db` → `shared`; `worker` is a standalone binary.

- **`crates/shared`** — domain models (`models.rs`) and password hashing. Every API-facing model derives `Serialize + JsonSchema + ts_rs::TS`; these three derives keep the OpenAPI spec and the TypeScript types in sync with the Rust structs.
- **`crates/db`** — all SQL lives here as `sqlx` query functions under `src/queries/` (one module per area: auth, users, scrobbles, tracks, enrichment, community, classification, imports), plus the migration runner in `src/migrate.rs`. Handlers never write inline SQL (exception: a couple of one-offs in handlers use `sqlx::query_scalar!` directly).
- **`crates/api`** — Axum 0.8 HTTP layer:
  - `router.rs` merges four route groups into one app: authed routes behind `require_auth`, a separate authed **upload** group with a larger `DefaultBodyLimit` (8 MiB, for multipart image uploads), public routes, and user routes behind `optional_auth` (injects `AuthUser` if a valid Bearer token is present, without requiring one — needed for things like `is_following` on public profiles and `has_voted` on image candidates). Uploaded images are served statically from `/uploads` via `ServeDir`. Global layers: rate limiting (60 requests a minute per client IP: the peer address, or with `TRUSTED_PROXY_HOPS=N` the `X-Forwarded-For` entry the outermost of N proxies appended; the API is served with `ConnectInfo` for this), tracing, gzip, permissive CORS.
  - `middleware/` — `auth.rs` (session/API-token auth, inserts `AuthUser` into request extensions; handlers extract it with `Extension(auth_user)`), `rate_limit.rs`, `visibility.rs` (enforces `is_private` profiles).
  - `errors.rs` — `AppError` enum with `IntoResponse` mapping to status codes; all handlers return `ApiResult<T>`. Database/Redis/Internal variants log and return opaque 500s.
  - OpenAPI docs via `aide`: every handler has a sibling `_<name>_doc(TransformOperation)` function registered in the router. Spec served at `/api.json`, Scalar UI at `/docs`.

### Auth model

Two credential types, both resolved by the auth middleware:
- **Sessions**: UUID tokens in `user_sessions`, cached in Redis under `session:{id}` (logout must invalidate both).
- **API tokens**: long-lived, scoped (e.g. `scrobble`), stored hashed via `auth_db::hash_api_token`; the raw token is shown only once at creation.

### Metadata enrichment

The worker runs an enrichment pipeline (`crates/worker/src/enrichment/`) over the catalog: MusicBrainz (MBIDs, durations, release dates — 1 req/s hard limit), Cover Art Archive (album covers by MBID), Deezer (artist images + cover fallback), Last.fm (bios, only when `LASTFM_API_KEY` is set). Jobs live in `enrichment_jobs` (queue queries in `db/src/queries/enrichment.rs`), enqueued at ingest for never-enriched entities, by the `POST /v1/{track,artist,album}/{id}/refresh` endpoints, and by periodic backfill/re-sweeps. Merge policy: fill-only-NULL; `mbid` is never overwritten; images/bio are overwritten only on forced refresh; names/titles are never touched; and an image is never touched when `image_locked` is set (a community-voted or user-uploaded image — see below). Provider rate limiters are in-process — run a single worker instance.

The worker also holds an optional Redis client (best-effort — a missing/unreachable `REDIS_URL` only disables this, enrichment still runs): after it fills an artist/album image, it re-publishes `now_playing` over the API's SSE channel for anyone currently playing that entity, so a live now-playing card swaps its fallback for the real cover within seconds instead of showing the pre-enrichment placeholder for the whole track.

### Scrobble classification (anti-botting, shadow mode)

The worker labels every scrobble `counted`, `suspect`, `duplicate` or `no_data` after ingest so rankings can later ignore bot plays (`suspect` and `duplicate` are never meant to count). **Shadow mode: nothing reads the labels yet.** No chart, cagg, query or endpoint changes, and ingest never rejects anything for being suspicious. The pieces: the pure rule in `shared::classification` (offline unit tests), storage and queue in `db::queries::classification` (schema in migrations `0010` and `0012`), and the worker loops plus CLI in `crates/worker/src/classification/`.

- **Lengths:** the rule knows up to three per scrobble: MusicBrainz's `tracks.mb_duration_ms`, the catalog's `tracks.duration_ms` (first client, or Last.fm) and the play's own `scrobbles.duration_ms`. Any can be wrong (MusicBrainz matches snippets, live takes and medleys; crowd-sourced lengths are off), so none is the truth: listening time uses the **longest**, so a wrong short one can't be farmed, and the repeat window the **shortest**, so a wrong long one can't swallow real replays.
- **Duplicates:** a scrobble of the track the user scrobbled less than `min(0.9 × shortest length, 4 min)` before (30 s without a length) is `duplicate`: the same listen reported again by several scrobblers or a retrying one, seconds to ~4 minutes apart in real histories. It uses no budget; copies chain.
- **Rule (listening-time budget):** a scrobble is suspect when the listening time of all scrobbles in the window ending at it, itself included, exceeds `window × max_ratio + slack` (defaults 1 h × 2.0 + 15 min, from `CLASSIFIER_WINDOW_SECS` / `CLASSIFIER_MAX_RATIO` / `CLASSIFIER_SLACK_SECS`; invalid values stop the worker). A scrobble's listening time is the track length, or `scrobbles.listened_ms` when set, clamped between `min(30 s, length/2)` and the length, and capped at one window. When the next scrobble that isn't a duplicate starts before a play could have ended, the play was skipped: from then on it counts the gap, but never less than its scrobble point `min(length/2, 4 min)`. A label therefore depends only on earlier scrobbles. Without any length the scrobble is `no_data`: it uses no budget, so it never counts against anyone.
- **Ceiling:** clients without listened time get at most budget ÷ scrobble point plays counted per window (~67 an hour of 4-minute tracks) and one per repeat window of the same track; clients reporting `listened_ms` can go down to 30 s a play (~270 an hour).
- **Storage is sparse, per user and UTC day:** `scrobble_classification_days` (one row per classified day, with counts and the `classifier_rulesets` id) plus `scrobble_flags` (only non-counted scrobbles, with reason, duration source, occupancy and window load). The `scrobble_labels` view gives one label per scrobble (NULL = not classified yet). Classification only **reads** the `scrobbles` hypertable, so reclassifying compressed history never decompresses chunks. A ruleset is fingerprinted by `RULES_VERSION` and the thresholds: **bump `RULES_VERSION` when the rule's logic changes**, and every stored day becomes stale and is reclassified in the background.
- **Keeping it fresh:** ingest (`ingest_scrobble`, hence also the Spotify poller) queues the scrobble's day with `ON CONFLICT DO NOTHING` and a 30 s settle, so a burst costs one index probe per scrobble. When enrichment first learns a track's MusicBrainz length, it queues the track's plays from the last 30 days. A 5-minute sweep queues days that are missing, classified under another ruleset, or whose count no longer matches `user_activity_daily`. That covers crashed claims and dropped days, but a late insert into an already-materialized bucket reaches it only after the hourly aggregate refresh. The same sweep re-checks `no_data` flags whose track has since gained a length (a new length only ever adds listening time, so it can't clear a suspect). A day whose tail changes queues the next day, whose lookback (one window plus 4 min, for duplicates) it is.
- **Imports:** after inserting history, call `classification::enqueue_scrobble_classification(pool, user_id, from, to)`, alongside `scrobbles::refresh_scrobble_aggregates`.
- **CLI:** `worker classify report [--all] [--user NAME]` shows coverage, totals per ruleset, the users with most suspect and most duplicate scrobbles, clients reporting lengths far below the catalog's or MusicBrainz's, and tracks whose two lengths disagree; with `--user`, that user's worst days. It covers the last 30 days unless given `--from` or `--all`. `reclassify [--user] [--from] [--to] [--dry-run]` recomputes synchronously and prints label transitions; run it with different `CLASSIFIER_*` values plus `--dry-run` to preview a threshold change. `backfill [--dry-run]` queues every missing or stale day.

### Last.fm history import

How users bring their Last.fm history over: `POST /v1/import/lastfm` (API) or `worker import lastfm --user NAME --lastfm USER` (operator CLI, `just import-lastfm`). The pieces: the client and cursor in `shared::lastfm`, dedup in `shared::import`, jobs and the page transaction in `db::queries::imports` (migration `0011`), the runner and CLI in `crates/worker/src/lastfm_import/`, and track lengths in `crates/worker/src/enrichment/lengths.rs`.

- **Jobs** (`scrobble_imports`): at most one active per user. A worker leases a job for 25 pages at a time, so imports take turns, and every page commits together with the job's cursor. A crash, restart or expired lease therefore resumes at the next page. The walk goes newest to oldest under a window fixed when the job starts. It runs in segments of 20 pages, each restarting at page 1 one second above the oldest scrobble seen; that avoids deep page offsets and is correct whether Last.fm's `to` is inclusive or not.
- **Errors:** 29 (rate limit) cools every Last.fm caller down for 60 s without spending an attempt. Other transient errors back off from 30 s to 1 h, and the job fails after 10 attempts. 6 fails it as `user_not_found` and 17 as `history_hidden` (a verified import signs requests with the user's session, which can read a hidden history). `LASTFM_IMPORT_MAX_SCROBBLES` fails it as `cap_reached`.
- **Writes** (`record_page`, one transaction per page):
  - Catalog upserts are set-based and use ingest's normalization; new tracks get their primary credit.
  - Dedup (`shared::import::new_plays`) compares against the user's stored rows in the page's time span. It skips an earlier import's row with the same second and track, and a live scrobble of the same track within 10 min, matched one-for-one.
  - Rows are inserted with `source = 'lastfm_import'` and `import_id`. `import_id` is set by the server, so rules should trust it rather than `source`.
  - Counters are updated set-based: `increment_scrobble_counts` skips rows with `import_id`, and `last_seen_at` only moves forward.
  - Old history lands in the uncompressed part of compressed chunks, and the compression policy recompresses them. The primary-key uniqueness check decompresses overlapping batches in memory, so insert cost grows with the number of users active in that week.
- **Checkpoints** (segment boundary, end of a lease's pages, end of the job; tracked as `pending_from`/`pending_to`) — never per scrobble:
  - `refresh_scrobble_aggregates`, retried while the scheduled refresh holds the lock;
  - `enqueue_scrobble_classification`;
  - enrichment for new catalog entries at priority 20–29 by play count, below live ingest.
- **Re-imports** start 14 days before the last completed import's `window_to`, since Last.fm accepts scrobbles up to two weeks late. `full` rescans everything. Neither duplicates anything.
- **Track lengths:** Last.fm history has none, which would leave the classifier with only `no_data`.
  - A backfill asks `track.getInfo` (2/s inside the shared 4/s Last.fm limiter) about tracks with no length anywhere, most scrobbled first. It fills the catalog `duration_ms` and doesn't ask again for 30 days after a miss.
  - The MusicBrainz track job looks up the recording mbid Last.fm reported (`tracks.mbid_hint`) directly, adopting it only if its title and artist match.
  - The classifier's sweep relabels `no_data` days as lengths arrive.
- **Anti-abuse:**
  - API imports require connecting the Last.fm account through its web auth (`GET /v1/connect/lastfm`). That needs `LASTFM_SHARED_SECRET` and `TOKEN_ENCRYPTION_KEY`, and `connected_accounts` allows one Scrobblr user per Last.fm account. These imports are `verified`.
  - API imports are limited to one a day per user and are capped.
  - Every imported day goes through classification, and `import_id` lets rankings discount imports later.
  - CLI imports are trusted as coming from an operator and are stored with `verified = false`.
- **Tests:** the worker's tests run imports against an in-process fake Last.fm (`crates/worker/src/fake_lastfm.rs`), never the real API.

### Community contributions (uploads, image voting, comments)

`crates/api/src/handlers/uploads.rs` + `community.rs`, backed by `db/src/queries/community.rs`. Multipart image uploads are decoded, downscaled to ≤1024px and re-encoded as JPEG (strips EXIF; source dimensions capped before decode to bound memory), stored under `UPLOAD_DIR` and served from `/uploads`; stored URLs are built from `PUBLIC_BASE_URL` (both env vars — the API warns and falls back to `http://localhost:8080` if `PUBLIC_BASE_URL` is unset).

- **Avatars** (`POST /v1/user/me/avatar`) replace the user's own `image_url` directly. Because a stale avatar file is deleted on replace, `PATCH /v1/user/me` rejects an `image_url` that points at our own `/uploads/` path — otherwise a user could aim it at someone else's uploaded file and have the next avatar upload delete it.
- **Artist/album art** is last.fm-style community voting, **add-only**: an upload creates a row in `image_candidates` (uploader auto-likes their own). Likes go through `image_candidate_votes`; the most-liked candidate becomes the entity's displayed image once it reaches `MIN_VOTES_TO_DEFAULT` (3), which sets `image_url` + `image_locked`. Promotion only — a withdrawn like never demotes the shown image.
- **Comments** (`comments` table) attach to artists or tracks; reads are public, writes authed, deletes owner-only.

Listener/social sections and the private-profile rules: `artist_listeners`/`track_listeners`/`search_users` all exclude `is_private` users, so a private account stays undiscoverable in aggregate surfaces (its profile also 403s via `middleware/visibility.rs`).

### Track credits (multiple artists)

`tracks.artist_id` remains the **primary** artist: it backs `UNIQUE (artist_id, title_normalized)` and the denormalized `scrobbles.artist_id` every aggregate query reads. `track_artists` (migration `0007`) holds the full credit list, one row per artist with a `track_artist_role` of `primary` or `featured` plus a billing `position`; a partial unique index keeps at most one `primary` per track, and the `primary` row always mirrors `tracks.artist_id`. Chosen over a `BIGINT[]` column because Postgres can't foreign-key array elements.

Credits are written by `tracks_db::record_track_credits` from ingest (`/v1/scrobble`, `/v1/now-playing`, and the Spotify poller, which maps `track.artists[0]` to primary and the rest to featured) and are **add-only** — a client omitting a collaborator never erases one. Aggregate surfaces (`artist_top_tracks`, `artist_listeners`, search) deliberately stay primary-artist-only; making featured credits count there means rewriting them to join `track_artists`.

### Scrobble clients

`scrobbles.source` is whatever the client claims. `scrobbles.client_id` (migration `0013`) points at `scrobble_clients`: the protocol the scrobble arrived by, the client as that protocol identifies it, and `verified` when the server checked that identity. Native `/v1/scrobble` records `scrobblr` with its claimed source (unverified), the Spotify poller `spotify` (verified). Ingest callers pass `ScrobbleInput.client_id`; the API resolves ids through `AppState.clients`, a bounded cache, since names are client-chosen. Imports leave it NULL (`import_id` already says where they came from).

### Auth input rules and client attestation

`shared::validation` owns every credential rule (username charset/length, RFC-lite email structure, password length + complexity, display-name sanitization) and is enforced **only at registration** — `login` applies just the bounds needed to keep a hostile body away from Argon2, since re-applying the current rules would lock out older accounts. `login` also verifies against a decoy hash when no user matches, so response latency can't be used to enumerate usernames.

`middleware/app_signature.rs` gates `/v1/auth/register` and `/v1/auth/login` behind an HMAC-SHA256 request signature when `AUTH_APP_KEYS` is set (unset = open, so existing clients keep working). Nonces are burned in Redis, and a Redis failure rejects rather than passes.

### Username semantics

Lookups (`find_by_username`, `find_by_email`) are case-insensitive (`lower(col) = lower($1)`), but the DB UNIQUE constraint on `users.username` is case-sensitive — keep the two in mind when touching registration/lookup code. `RESERVED_USERNAMES` in `handlers/auth.rs` blocks names that collide with route literals like `/user/me`, checked case-insensitively.

### Mobile app (apps/mobile)

Flutter Android scrobbler. Architecture is documented in `apps/mobile/README.md`; the short version: a Kotlin `NotificationListenerService` uses `MediaSessionManager` to observe every player and forwards raw events into a headless background FlutterEngine; the pure-Dart pipeline in `lib/scrobbling/` (per-source parsers keyed by package name, debounce/dedupe state machine, offline retry queue) is where all behavior lives and is what `flutter test` covers. The UI reads with the session token; the background scrobbler uses a provisioned API token (scope `scrobble`). Dart API models in `lib/api/models.dart` mirror `crates/shared/src/models.rs` **by hand** — update them when API-facing Rust models change (the ts_rs pipeline only covers TypeScript).

### Types pipeline (Rust → TypeScript)

`#[ts(export)]` on shared models + `TS_RS_EXPORT_DIR = packages/types/src/generated` (set in `.cargo/config.toml`) means **running `cargo test` regenerates the TS bindings** in `packages/types`, which `apps/extension` consumes via the `types` workspace package. If you change a shared model, run `cargo test -p shared` and commit the regenerated files.
