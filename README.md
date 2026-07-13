# newfm

> A music scrobbling service — a modern, self-hosted alternative to last.fm.

## Architecture

```
newfm/
├── crates/
│   ├── api/      ← Axum HTTP API (handlers, middleware, router, uploads)
│   ├── shared/   ← Domain models & password hashing (source of generated TS types)
│   ├── db/       ← SQLx queries (repositories for all entities)
│   └── worker/   ← Background jobs (cleanup, metadata enrichment, now-playing republish)
├── apps/
│   ├── extension/ ← Plasmo browser extension
│   └── mobile/    ← Flutter Android scrobbler (see apps/mobile/README.md)
├── packages/
│   └── types/    ← TypeScript types generated from crates/shared via ts-rs
├── migrations/    ← numbered plain-SQL, applied in order (0001 … 0005)
└── .env.example
```

**Stack:** Rust · Axum 0.8 · SQLx 0.8 · PostgreSQL + TimescaleDB · Redis (fred) · Flutter (mobile) · Bun + Turbo + Biome (JS tooling)

---

## Prerequisites

Either [devenv](https://devenv.sh) (recommended — provides everything below, including PostgreSQL and Redis services), or:

- Rust (stable, 2024 edition)
- PostgreSQL with [TimescaleDB](https://docs.timescale.com/self-hosted/latest/install/) extension
- Redis
- [Bun](https://bun.sh) (only for the extension / JS packages)

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

The API comes up on http://localhost:8080 (docs at `/docs`). All migrations are
applied automatically on first init; later ones are applied by hand (see below).
Point the web app / mobile app at this origin.

### With devenv

```bash
cp .env.example .env
devenv up        # starts PostgreSQL (only 0001 applied on first init) and Redis
# apply any later migrations manually (see below), then:
cargo run -p api
```

### Manual

```bash
# 1. Copy and edit the env file
cp .env.example .env

# 2. Create the database and apply every migration in order
createdb newfm
for f in migrations/0*.sql; do psql newfm -f "$f"; done

# 3. Run the API
cargo run -p api

# 4. (Optional) Run the background worker (enrichment + now-playing republish)
cargo run -p worker
```

There is no migration runner — the numbered files in `migrations/` are applied manually, in order. Re-run new ones after pulling schema changes.

Interactive API docs are served at [`/docs`](http://localhost:8080/docs) (OpenAPI spec at `/api.json`).

---

## Development

```bash
just fmt        # cargo fmt --all
just lint       # clippy with -D warnings
just check      # cargo check --workspace
just ci         # fmt-check + lint + check + build

cargo test      # also regenerates packages/types from crates/shared (ts-rs)
```

SQLx query macros compile against the committed `.sqlx/` cache, so no database is needed to build. After adding or changing a query, run `cargo sqlx prepare --workspace` (requires a live `DATABASE_URL`) and commit the updated cache.

---

## Environment Variables

| Variable             | Required | Default                  | Description                         |
|----------------------|----------|--------------------------|-------------------------------------|
| `DATABASE_URL`       | ✓        | —                        | PostgreSQL connection string        |
| `REDIS_URL`          | —        | `redis://127.0.0.1:6379` | Redis (sessions; worker now-playing republish) |
| `BIND_ADDR`          | —        | `0.0.0.0:8080`           | API listen address                  |
| `PUBLIC_BASE_URL`    | —        | `http://localhost:8080`  | Base URL uploaded-image URLs are built from — set to your public origin |
| `UPLOAD_DIR`         | —        | `uploads`                | Directory user-uploaded images are written to and served from |
| `RUST_LOG`           | —        | —                        | Tracing filter (e.g. `newfm=debug`) |
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

Uploaded images are re-encoded to JPEG (EXIF stripped, downscaled, source dimensions capped), stored under `UPLOAD_DIR`, and served from `/uploads`. Private profiles are excluded from search and listener lists.
