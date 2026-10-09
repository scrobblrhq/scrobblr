# Scrobblr

> A music scrobbling service — a modern, self-hosted alternative to last.fm.

This is the **backend** repo (Rust API + worker + shared types). The clients live
in sibling repos: [scrobblrhq/extension](https://github.com/scrobblrhq/extension)
(browser) and [scrobblrhq/mobile](https://github.com/scrobblrhq/mobile) (Flutter);
both consume the [`@scrobblr/types`](https://www.npmjs.com/package/@scrobblr/types)
package published from here.

## Architecture

```
scrobblr/
├── crates/
│   ├── api/      ← Axum HTTP API (handlers, middleware, router, uploads)
│   ├── shared/   ← Domain models & password hashing (source of generated TS types)
│   ├── db/       ← SQLx queries (repositories for all entities)
│   └── worker/   ← Background jobs (cleanup, metadata enrichment, now-playing republish,
│                   scrobble classification) and the `migrate` / `classify` CLI
├── packages/
│   └── types/    ← @scrobblr/types — TS types generated from crates/shared via ts-rs
├── migrations/    ← numbered plain-SQL, applied in order by `just migrate`
├── Dockerfile · docker-compose.yml   ← self-host / shared dev backend
└── .env.example
```

**Stack:** Rust · Axum 0.8 · SQLx 0.8 · PostgreSQL + TimescaleDB · Redis (fred) · Bun + Biome (JS tooling for the types package)

---

## Prerequisites

Either [devenv](https://devenv.sh) (recommended — provides everything below, including PostgreSQL and Redis services), or:

- Rust (stable, 2024 edition)
- PostgreSQL with [TimescaleDB](https://docs.timescale.com/self-hosted/latest/install/) extension
- Redis
- [Bun](https://bun.sh) (only for the `@scrobblr/types` package)

---

## Getting started

### With Docker (self-host / shared dev backend)

The quickest way to run the whole backend (Postgres + TimescaleDB, Redis, API,
worker) — and the recommended way for web/mobile devs to get a backend without
the Rust toolchain:

```bash
cp .env.docker.example .env.docker   # then edit the passwords
docker compose --env-file .env.docker up -d --build
```

The API comes up on http://localhost:8080 (docs at `/docs`). A one-shot
`migrate` service applies pending migrations before the API and worker start.
Point the web app / mobile app at this origin. With `--profile caddy
--profile web`, Caddy serves the web app, the API and the uploads over HTTPS
on their own hosts ([docs/web-integration.md](docs/web-integration.md)).

### With devenv

```bash
cp .env.example .env
devenv up        # starts PostgreSQL and Redis, then applies pending migrations
cargo run -p api
```

### Manual

```bash
# 1. Copy and edit the env file
cp .env.example .env

# 2. Create the database and apply every migration in order
createdb scrobblr
just migrate     # = SQLX_OFFLINE=true cargo run -p worker -- migrate

# 3. Run the API
cargo run -p api

# 4. (Optional) Run the background worker (enrichment, now-playing republish,
#    scrobble classification; `cargo run -p worker -- --help` lists its CLI)
cargo run -p worker
```

`just migrate` applies every pending file in `migrations/` in order and records
it in `schema_migrations`; run it again after pulling schema changes (the API
and worker refuse to start until you do). `just migrate status` lists them. A
database migrated by hand before the runner existed can be adopted with
`just migrate --baseline 9`.

### Importing a Last.fm history

Users import their own Last.fm account through the API: they connect it
(`GET /v1/connect/lastfm`, which needs `LASTFM_API_KEY`,
`LASTFM_SHARED_SECRET`, `TOKEN_ENCRYPTION_KEY` and the web app, `WEB_APP_URL`,
where Last.fm sends them back), then
`POST /v1/import/lastfm`, and follow its progress at `GET /v1/imports/{id}`.
The worker runs the import, fills in track lengths and classifies the
imported scrobbles. Operators can also import any account for an existing
user from the command line (`worker import`; `cargo run -p worker -- --help`
lists its commands).

Interactive API docs are served at [`/docs`](http://localhost:8080/docs) (OpenAPI spec at `/api.json`;
[`openapi.json`](openapi.json) is its committed snapshot, which `just test` keeps current).

### The web app

The web app (SvelteKit) calls the API from its server with the user's
session; the browser never calls the API. What it must send and the pages
it must have: [docs/web-integration.md](docs/web-integration.md).

### Scrobbling from other apps

Existing scrobblers (Web Scrobbler, Pano Scrobbler, Audioscrobbler 1.2 players,
ListenBrainz clients) can scrobble to Scrobblr by changing only the server
URL: see [docs/scrobbler-clients.md](docs/scrobbler-clients.md).

---

## Development

```bash
just fmt        # cargo fmt --all
just lint       # clippy with -D warnings
just test       # unit tests; also regenerates packages/types from crates/shared (ts-rs)
just test-db    # database tests (need Postgres and Redis; each creates its own database)
just ci         # what CI's first job runs: fmt-check, lint, test, types-check
just ci-db      # what CI's database job runs: migrate DATABASE_URL, test-db, sqlx-check
```

SQLx query macros compile against the committed `.sqlx/` cache (the justfile
sets `SQLX_OFFLINE=true`), so no database is needed to build. After adding or
changing a query, run `just sqlx-prepare` against a migrated `DATABASE_URL`
and commit the updated cache; CI fails when it is stale.

CI (`.github/workflows/ci.yml`) runs `just ci`, and `just ci-db` against a
TimescaleDB and a Redis service, on every push to `main` and every pull
request.

---

## Environment Variables

| Variable             | Required | Default                  | Description                         |
|----------------------|----------|--------------------------|-------------------------------------|
| `DATABASE_URL`       | ✓        | —                        | PostgreSQL connection string        |
| `REDIS_URL`          | —        | `redis://127.0.0.1:6379` | Redis (sessions; worker now-playing republish) |
| `BIND_ADDR`          | —        | `0.0.0.0:8080`           | API listen address                  |
| `PUBLIC_BASE_URL`    | —        | `http://localhost:8080`  | Public origin of the API, for links it builds to itself — set to your public origin |
| `WEB_APP_URL`        | —        | —                        | The web app's origin, where browsers approve scrobblers and link Spotify and Last.fm accounts |
| `TRUSTED_PROXY_HOPS` | —        | `0`                      | Proxies whose `X-Forwarded-For` names the client: `1` behind `deploy/Caddyfile` and the web app |
| `TRUSTED_PROXIES`    | —        | loopback, private networks | The peers `X-Forwarded-For` is believed from |
| `CORS_ALLOWED_ORIGINS` | —      | none                     | Origins whose pages may call the native API from a browser, or `*` |
| `UPLOAD_DIR`         | —        | `uploads`                | Directory user-uploaded images are written to |
| `UPLOAD_PUBLIC_URL`  | —        | `{PUBLIC_BASE_URL}/uploads` | Base URL serving `UPLOAD_DIR`, e.g. a CDN host ([docs/uploads.md](docs/uploads.md)) |
| `RUST_LOG`           | —        | —                        | Tracing filter (e.g. `api=debug,worker=debug,sqlx=warn`) |
| `DB_MAX_CONNECTIONS` | —        | `20`                     | Postgres pool size                  |
| `LASTFM_API_KEY`     | —        | —                        | Enables artist bios (worker)        |

---

## Metadata enrichment

The worker enriches the catalog in the background: **MusicBrainz** resolves MBIDs, track durations and release dates (canonical source, 1 req/s); **Cover Art Archive** provides album covers by MBID; **Deezer** fills artist images and covers CAA lacks; **Last.fm** adds artist bios when `LASTFM_API_KEY` is set.

Jobs are queued in `enrichment_jobs` when new catalog entities are first scrobbled, by the authenticated `POST /v1/{track,artist,album}/{id}/refresh` endpoints (forced re-fetch), and by a periodic backfill sweep. Transient provider failures retry with exponential backoff; existing fields are never overwritten except images/bio on manual refresh, and never for a community-chosen image (`image_locked`). After the worker fills an artist/album image it re-publishes the affected users' now-playing over the live SSE stream, so an in-progress track's cover updates from its placeholder without waiting for the next song (requires `REDIS_URL`; degrades gracefully without it).

## Community contributions

- **Avatars** — users upload their own via `POST /v1/user/me/avatar`.
- **Artist/album artwork** — last.fm-style, add-only: uploads become candidates that users vote on; the most-liked candidate becomes the displayed image once it reaches 3 likes, and is then protected from enrichment overwrites.
- **Comments** — public reads, authenticated writes, owner-only deletes on artists and tracks.

Uploaded images are re-encoded to JPEG (EXIF stripped, downscaled, source dimensions capped) and stored under `UPLOAD_DIR`; the database keeps each one's key, and clients get `{UPLOAD_PUBLIC_URL}/{key}`. The API serves them at `/uploads`, or a static server on its own host can: see [docs/uploads.md](docs/uploads.md). Private profiles are excluded from search and listener lists.
