# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Scrobblr — a music scrobbling service (self-hosted last.fm alternative). This is the **backend** repo: a Rust workspace (Axum + SQLx + PostgreSQL/TimescaleDB + Redis) plus the `@scrobblr/types` package (TypeScript types generated from the Rust models via ts-rs, published to npm). The browser extension and Flutter app live in sibling repos (`scrobblrhq/extension`, `scrobblrhq/mobile`) and consume `@scrobblr/types` from npm.

## Commands

Dev environment is managed with devenv (nix): `devenv up` starts PostgreSQL (with TimescaleDB, DB `scrobblr` initialized from `migrations/0001_initial.sql`) and Redis (password `123`). `.env` is loaded automatically (dotenv is enabled in devenv and via `dotenvy` at runtime).

```bash
cargo run -p api          # run the API (requires DATABASE_URL; listens on BIND_ADDR, default 0.0.0.0:8080)
cargo run -p worker       # background jobs (session/now_playing cleanup, metadata enrichment, now-playing SSE republish, scrobble classifier)
cargo run -p worker -- classify report|reclassify|backfill   # internal shadow-mode review CLI (see below)

just fmt                  # cargo fmt --all
just lint                 # cargo clippy --workspace --all-targets -- -D warnings
just lint-fix             # clippy --fix
just check                # cargo check --workspace
just ci                   # fmt-check + lint + check + build

cargo test --workspace    # also regenerates TS bindings (see Types pipeline below)
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

Query macros (`sqlx::query!` etc.) compile against the `.sqlx/` cache, so no database is needed to build. When you add or change a query, you need a live `DATABASE_URL` and must run `cargo sqlx prepare --workspace` (sqlx-cli is in the devenv shell) and commit the updated `.sqlx/` files.

### Migrations

Numbered plain-SQL files applied **in order**: `0001_initial.sql`, `0002_enrichment.sql` (enrichment columns + `enrichment_jobs`), `0003_uploads.sql` (`image_locked` on artists/albums), `0004_community.sql` (`image_candidates`, `image_candidate_votes`, `comments`), `0005_scrobbles_artist_index.sql`, `0006_connected_accounts.sql` (`connected_accounts` — third-party OAuth connections), `0007_track_artists.sql` (`track_artists` + the `track_artist_role` enum), `0007_scrobble_classification.sql` (shadow-mode anti-botting: `classification_rulesets`, `scrobble_classifications`, `classification_days`, `classification_track_refresh` + a trigger on `tracks`). Automatic migration on API startup is **commented out** in `crates/api/src/main.rs`; there is no migration runner — apply each file manually with `psql scrobblr -f migrations/000N_*.sql`. devenv only initializes `0001` on first DB init, so after pulling schema changes you must apply the newer files yourself. (The README's mention of a `crates/core` crate is stale — the actual crate is `crates/shared`.)

## Architecture

Rust workspace crates and their dependency direction: `api` → `db` → `shared`; `worker` is a standalone binary.

- **`crates/shared`** — domain models (`models.rs`) and password hashing. Every API-facing model derives `Serialize + JsonSchema + ts_rs::TS`; these three derives keep the OpenAPI spec and the TypeScript types in sync with the Rust structs.
- **`crates/db`** — all SQL lives here as `sqlx` query functions under `src/queries/` (one module per area: auth, users, scrobbles, tracks, enrichment, community, connected_accounts, classification). Handlers never write inline SQL (exception: a couple of one-offs in handlers use `sqlx::query_scalar!` directly).
- **`crates/api`** — Axum 0.8 HTTP layer:
  - `router.rs` merges four route groups into one app: authed routes behind `require_auth`, a separate authed **upload** group with a larger `DefaultBodyLimit` (8 MiB, for multipart image uploads), public routes, and user routes behind `optional_auth` (injects `AuthUser` if a valid Bearer token is present, without requiring one — needed for things like `is_following` on public profiles and `has_voted` on image candidates). Uploaded images are served statically from `/uploads` via `ServeDir`. Global layers: rate limiting, tracing, gzip, permissive CORS.
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

### Community contributions (uploads, image voting, comments)

`crates/api/src/handlers/uploads.rs` + `community.rs`, backed by `db/src/queries/community.rs`. Multipart image uploads are decoded, downscaled to ≤1024px and re-encoded as JPEG (strips EXIF; source dimensions capped before decode to bound memory), stored under `UPLOAD_DIR` and served from `/uploads`; stored URLs are built from `PUBLIC_BASE_URL` (both env vars — the API warns and falls back to `http://localhost:8080` if `PUBLIC_BASE_URL` is unset).

- **Avatars** (`POST /v1/user/me/avatar`) replace the user's own `image_url` directly. Because a stale avatar file is deleted on replace, `PATCH /v1/user/me` rejects an `image_url` that points at our own `/uploads/` path — otherwise a user could aim it at someone else's uploaded file and have the next avatar upload delete it.
- **Artist/album art** is last.fm-style community voting, **add-only**: an upload creates a row in `image_candidates` (uploader auto-likes their own). Likes go through `image_candidate_votes`; the most-liked candidate becomes the entity's displayed image once it reaches `MIN_VOTES_TO_DEFAULT` (3), which sets `image_url` + `image_locked`. Promotion only — a withdrawn like never demotes the shown image.
- **Comments** (`comments` table) attach to artists or tracks; reads are public, writes authed, deletes owner-only.

Listener/social sections and the private-profile rules: `artist_listeners`/`track_listeners`/`search_users` all exclude `is_private` users, so a private account stays undiscoverable in aggregate surfaces (its profile also 403s via `middleware/visibility.rs`).

### Track credits (multiple artists)

`tracks.artist_id` remains the **primary** artist: it backs `UNIQUE (artist_id, title_normalized)` and the denormalized `scrobbles.artist_id` every aggregate query reads. `track_artists` (migration `0007`) holds the full credit list, one row per artist with a `track_artist_role` of `primary` or `featured` plus a billing `position`; a partial unique index keeps at most one `primary` per track, and the `primary` row always mirrors `tracks.artist_id`. Chosen over a `BIGINT[]` column because Postgres can't foreign-key array elements.

Credits are written by `tracks_db::record_track_credits` from ingest (`/v1/scrobble`, `/v1/now-playing`, and the Spotify poller, which maps `track.artists[0]` to primary and the rest to featured) and are **add-only** — a client omitting a collaborator never erases one. Aggregate surfaces (`artist_top_tracks`, `artist_listeners`, search) deliberately stay primary-artist-only; making featured credits count there means rewriting them to join `track_artists`.

### Auth input rules and client attestation

`shared::validation` owns every credential rule (username charset/length, RFC-lite email structure, password length + complexity, display-name sanitization) and is enforced **only at registration** — `login` applies just the bounds needed to keep a hostile body away from Argon2, since re-applying the current rules would lock out older accounts. `login` also verifies against a decoy hash when no user matches, so response latency can't be used to enumerate usernames.

`middleware/app_signature.rs` gates `/v1/auth/register` and `/v1/auth/login` behind an HMAC-SHA256 request signature when `AUTH_APP_KEYS` is set (unset = open, so existing clients keep working). Nonces are burned in Redis, and a Redis failure rejects rather than passes.

### Scrobble classification (anti-botting, shadow mode)

`crates/worker/src/classifier.rs` + `db/src/queries/classification.rs`, rule in `shared/src/classification.rs`. Every scrobble gets a label (`counted` / `suspect` / `no_data`, a reason, the ruleset id, a score) **after** ingest. **Shadow mode:** nothing reads the labels — no chart, ranking, query or endpoint uses them and nobody is blocked; ingest never rejects for being suspicious. Labels live in side tables, not on `scrobbles` (compressed-chunk updates and cagg invalidations).

- **Rule 1, time budget:** for each scrobble, durations (each clipped to the window W) of the scrobbles in `(t − W, t]` must not exceed `(W + own duration) × CLASSIFIER_MARGIN_RATIO + CLASSIFIER_MARGIN_SLACK_SECS`. Unknown duration → `no_data`, never suspect, adds nothing to neighbours. Extension + Spotify copies of the same play (same track, different source, within the play's duration) are paired one-to-one and counted once. Duration = `COALESCE(tracks.duration_ms, scrobbles.duration_ms)` (note: `scrobbles.duration_ms` holds the client-reported *track* length, despite its column comment).
- **Rulesets:** `RULES_VERSION` (bump when the rule's logic changes) + the env thresholds. On startup the worker upserts and activates its ruleset and re-queues every day labeled under another one (background priority) — so changing a threshold reclassifies history. Run a single worker: two workers with different configs would keep re-queueing each other's days.
- **Ledger:** `classification_days`, one row per (user, UTC day): `dirty` + `dirty_gen` (a mark during processing keeps the day dirty), `next_attempt_at` + the worker's settle delay, a claim lease (crashed worker → reclaimable after 15 min), per-status counts. Marked by: a best-effort call in `ingest_scrobble` (day + next day if within W of midnight; W comes from the active ruleset, since the API has no classifier config); a 6h reconciliation of the last 3 days (catches dropped marks — a dropped mark on an import older than that needs `classify backfill`); the `tracks` trigger when a duration goes NULL→value (re-checks `no_data`); the CLI.
- **CLI:** `worker classify report [--top N]` (distribution, ledger state, top suspect users incl. private ones), `reclassify [--user NAME | --user-id ID] [--from RFC3339] [--to RFC3339]`, `backfill` (queue all existing history — run once after applying 0007 on a database with history). These only mark days; the running worker classifies.
- **Tests:** rule scenarios are offline unit tests. DB tests are `#[ignore]`d in `classifier.rs` and must run serially against a scratch DB (they flip the active ruleset and add temporary `CHECK (false)` constraints): `createdb scrobblr_test`, apply 0001–0007, then `DATABASE_URL=postgresql://localhost:5432/scrobblr_test cargo test -p worker -- --ignored --test-threads=1`.

### Username semantics

Lookups (`find_by_username`, `find_by_email`) are case-insensitive (`lower(col) = lower($1)`), but the DB UNIQUE constraint on `users.username` is case-sensitive — keep the two in mind when touching registration/lookup code. `RESERVED_USERNAMES` in `handlers/auth.rs` blocks names that collide with route literals like `/user/me`, checked case-insensitively.

### Mobile app (apps/mobile)

Flutter Android scrobbler. Architecture is documented in `apps/mobile/README.md`; the short version: a Kotlin `NotificationListenerService` uses `MediaSessionManager` to observe every player and forwards raw events into a headless background FlutterEngine; the pure-Dart pipeline in `lib/scrobbling/` (per-source parsers keyed by package name, debounce/dedupe state machine, offline retry queue) is where all behavior lives and is what `flutter test` covers. The UI reads with the session token; the background scrobbler uses a provisioned API token (scope `scrobble`). Dart API models in `lib/api/models.dart` mirror `crates/shared/src/models.rs` **by hand** — update them when API-facing Rust models change (the ts_rs pipeline only covers TypeScript).

### Types pipeline (Rust → TypeScript)

`#[ts(export)]` on shared models + `TS_RS_EXPORT_DIR = packages/types/src/generated` (set in `.cargo/config.toml`) means **running `cargo test` regenerates the TS bindings** in `packages/types`, which `apps/extension` consumes via the `types` workspace package. If you change a shared model, run `cargo test -p shared` and commit the regenerated files.
