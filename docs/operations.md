# Operations

Running the backend on one host with Docker Compose: `docker-compose.yml`
starts Postgres (TimescaleDB), Redis, a one-shot `migrate`, the API and the
worker. It ships no reverse proxy; put your own in front (below).

The commands assume the settings are in `.env` next to `docker-compose.yml`
(`cp .env.docker.example .env`, then set the passwords and URLs). On a
development machine, where `.env` belongs to `cargo run`, keep them in
`.env.docker` and add `--env-file .env.docker` to every `docker compose`
command: without it, Compose would recreate the containers with the default
passwords.

## Start, update, stop

```bash
git clone https://github.com/scrobblrhq/scrobblr.git && cd scrobblr
cp .env.docker.example .env        # edit it
docker compose up -d --build
```

`migrate` applies pending migrations, then the API and the worker start; they
refuse to run against an outdated schema. The API answers on
`http://127.0.0.1:8080` (`/health`, docs at `/docs`).
`docker compose ps` shows the services, `docker compose logs -f api worker`
their logs.

To update:

```bash
git pull
docker compose up -d --build
```

To stop: `docker compose stop`, or `docker compose down` to remove the
containers. **Never `docker compose down -v`**: `-v` deletes the volumes,
the database (`scrobblr_db-data`) and the uploaded images
(`scrobblr_uploads`) with them.

## Backups

The `backup` service backs up the database and the uploaded images every
day at `BACKUP_TIME` (UTC), into `BACKUP_DIR` on the host. It is built on
the database's own image, so its `pg_dump` and `pg_restore` are the
server's version, and it only reads: `pg_dump` runs in a read-only
transaction and the uploads volume is mounted read-only.

| Setting | Default | |
| --- | --- | --- |
| `BACKUP_DIR` | `./backups` | Host directory the backups go to. Put it on a disk with room for them, outside the checkout, e.g. `/var/backups/scrobblr` |
| `BACKUP_TIME` | `03:30` | When the daily backup runs, UTC |
| `BACKUP_KEEP_DAILY` | `7` | Days whose newest backup is kept |
| `BACKUP_KEEP_WEEKLY` | `4` | ISO weeks whose newest backup is kept |
| `BACKUP_MAX_AGE_HOURS` | `26` | Older than this, the last backup makes the service unhealthy |
| `BACKUP_REMOTE` | unset | An rclone destination each backup is copied to (below) |
| `BACKUP_PING_URL` | unset | Requested after each backup; `/fail` is appended when it failed |

Each backup is a directory `scrobblr-YYYYMMDDTHHMMSSZ` holding
`scrobblr.dump` (`pg_dump -Fc`), `uploads.tar`, a `manifest` (Postgres,
TimescaleDB and schema versions, upload count) and `SHA256SUMS`. It is
written as `.partial-…` and renamed only once verified: its checksums
match, `pg_restore` reads the whole dump and finds the `users` table in
it, and the archive holds as many files as were archived. A directory with
a backup's name is therefore complete; anything that fails ends with a
`backup: FAILED: <reason>` line in the log, no new backup, and an
unhealthy service. The database is dumped first and the uploads after:
an image is stored before a row refers to it, so the archive has every
file the dump names.

The files are readable by root only (`0600`): they hold password hashes
and encrypted account tokens. `TOKEN_ENCRYPTION_KEY` is not in them: keep
it apart from the backups and restore it with them, or linked Spotify and
Last.fm accounts must be connected again.

**Schedule and retention.** When no backup succeeded since the latest
`BACKUP_TIME` (the first start, or the host was down then), it backs up
at once; a failed backup is retried hourly. After each backup, it keeps the newest
backup of each of the `BACKUP_KEEP_DAILY` latest days and of each of the
`BACKUP_KEEP_WEEKLY` latest weeks that have one, and deletes the rest
(the newest is always kept). With the defaults that is about a month of
history, at most 11 backups. Every backup holds all uploads (they're
already JPEGs, so they're archived as they are): budget
`11 × (dump + uploads)` of disk.

**Locks.** `pg_dump` holds a share lock on every table until it ends.
Reads and writes go on; only what needs an exclusive lock waits for the
dump to finish: a migration, or TimescaleDB compressing a chunk of
scrobbles older than 30 days (its policy runs every 12 h; an import ends
with one too). While a compression waits, queries touching that old
chunk wait behind it, so for at most the dump's duration, old history
(profile pages reaching back, imports, reclassification) can stall; new
scrobbles and recent data are unaffected. The dump itself waits at most a
minute for its locks rather than queue behind a migration. Keep
`BACKUP_TIME` away from deploys, and watch how long a dump takes in
`docker compose logs backup`.

```bash
docker compose logs backup                     # what it did
docker compose exec backup scrobblr-backup run   # back up now (exit 1 on failure)
docker compose exec backup scrobblr-backup list  # the backups kept
docker compose exec backup scrobblr-backup check # what its healthcheck says
docker compose exec backup scrobblr-backup verify scrobblr-20261010T033000Z
```

### Off the machine

A backup on the same disk doesn't survive the disk. `BACKUP_REMOTE` names
an [rclone](https://rclone.org/) destination (any of its backends: SFTP,
S3-compatible storage, B2, another rsync-able host…); each backup is
copied there, checked against the local files, and the remote gets the
same retention. Only directories named like backups are ever deleted
there. A failed copy fails the backup (logged, unhealthy, retried).

rclone's configuration lives in `BACKUP_RCLONE_DIR` (default
`./backup/rclone`, mounted at `/config/rclone`), with whatever keys it
needs. To create it interactively:

```bash
docker compose run --rm --entrypoint rclone backup config
```

Then, for a remote named `offsite`, `BACKUP_REMOTE=offsite:scrobblr-backups`.
For SFTP with a key: put the key in `BACKUP_RCLONE_DIR`, and give
`key_file = /config/rclone/id_ed25519` in the remote's settings. Without
a config file, a remote can be given whole, as rclone's connection
strings allow: `BACKUP_REMOTE=:sftp,host=backup.example.com,user=scrobblr,key_file=/config/rclone/id_ed25519:scrobblr-backups`.
Test with `docker compose exec backup scrobblr-backup run`.

### Restore

Restoring replaces the database and the uploads with a backup's. The
`restore` service (never started by `up`) verifies the backup, refuses
while the API or worker is connected, and refuses a database that has
data unless given `--replace`. It recreates the database with the
backup's TimescaleDB version, loads the dump between TimescaleDB's
`timescaledb_pre_restore()` and `timescaledb_post_restore()`, runs
`ANALYZE`, and unpacks the uploads for the API's user (uid 10001).

```bash
docker compose stop api worker backup
docker compose up -d --wait db
docker compose run --rm restore scrobblr-20261010T033000Z --replace
docker compose up -d
```

`--replace` drops the current database and empties the uploads volume;
leave it out on a new host or empty volumes. `migrate` then applies any
migration newer than the backup. A backup that is only on the remote is
copied back first:

```bash
docker compose run --rm --entrypoint rclone backup \
  copy offsite:scrobblr-backups/scrobblr-20261010T033000Z /backups/scrobblr-20261010T033000Z
```

On a new host: clone the repository at a commit whose `docker-compose.yml`
pins the TimescaleDB version in the backup's `manifest` (the restore
refuses another), write `.env` (the passwords need not be the old ones;
`TOKEN_ENCRYPTION_KEY` must be), put the backup in `BACKUP_DIR`, then
`docker compose up -d --wait db` and the `restore` and `up -d` lines
above. `backup/test.sh` runs all of this end to end in a throwaway
project.

Without the service, the same steps by hand (a dump of the same
TimescaleDB version as the image):

```bash
docker compose up -d --wait db
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_pre_restore();"
docker compose exec -T db pg_restore -U scrobblr -d scrobblr --no-owner < scrobblr.dump
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_post_restore();"
docker run --rm -v scrobblr_uploads:/uploads -v "$PWD":/backup alpine \
  sh -c 'tar xf /backup/uploads.tar -C /uploads && chown -R 10001:10001 /uploads'
docker compose up -d
```

`pg_dump` warns about circular foreign keys on `continuous_agg`; that is
TimescaleDB's catalog and harmless with a full dump.

## Monitoring

What to watch from outside, cheapest first:

- **Uptime checks** (any external monitor: Uptime Kuma, UptimeRobot,
  Better Stack…), every minute or few, through the proxy:
  - `GET /health`: `200 ok` when the database and Redis answer within
    2 s; otherwise `503 unavailable: database, redis` (whichever is down;
    the error is in the API's log). Compose's healthcheck of the `api`
    uses it too.
  - `GET /health/worker`: `200 ok` while every worker loop keeps up,
    `503 unhealthy` otherwise (`unavailable` without the database); which
    loop, and why, is for `worker status`. Both count against the global
    rate limit of the monitor's address, like any request.
- **Backups**: `BACKUP_PING_URL`, a dead man's switch such as
  healthchecks.io, which alerts when a day passes without a ping or a
  `/fail` arrives.
- **`docker compose ps`** shows each service's health: `api` (`/health`),
  `worker` (`worker status`), `backup` (its last backup). Docker only
  reports it; nothing restarts an unhealthy container.

### Worker status

The worker's loops record each run in the database (`worker_heartbeats`).
A loop is **stalled** when it hasn't finished a run within
`WORKER_STALL_FACTOR` (default 3) times its interval (hung, or the worker
is gone), and **failing** when it runs but hasn't run without an error in
that time. Problems with outside services (MusicBrainz, Deezer, Last.fm,
Spotify) are logged but don't make a loop fail.

```text
$ docker compose exec worker worker status
loops (stalled or failing after 3x their interval):
  loop                       state      last run    last ok  interval  last error
  classification             ok          12s ago    12s ago        5m
  classification sweep       ok           3m ago     3m ago       10m
  cleanup                    ok           1m ago     1m ago        5m
  …
  rankings                   stalled     47m ago    47m ago        5m

queues (due now):
  classification       120, oldest waiting 46m
  ranking                0
  enrichment          1500, oldest waiting 2h10m
  import                 1, oldest waiting 3m

status: unhealthy (rankings)
```

It exits 1 when unhealthy. A loop that isn't configured (Spotify without
its keys, Last.fm imports without `LASTFM_API_KEY`) isn't listed. A queue
that keeps growing while its loop is `ok` is behind, not stuck: the
enrichment queue drains at MusicBrainz's 1 request a second. After a
stall, `docker compose logs worker` has the reason, and
`docker compose restart worker` a fresh start.

### Metrics

With `METRICS_TOKEN` set (16 characters or more, e.g.
`openssl rand -hex 32`), `GET /metrics` serves Prometheus text to requests
with `Authorization: Bearer <token>`, and is a 404 otherwise. It reads the
database at each scrape: scrape every 30 s or slower.

| Metric | |
| --- | --- |
| `scrobblr_up{dependency}` | `database` and `redis` answer (1) or not (0) |
| `scrobblr_http_responses_total{class}` | Responses by status class (`1xx`…`5xx`), since the API started |
| `scrobblr_http_rate_limited_total` | Responses with 429, since the API started |
| `scrobblr_worker_healthy` | 1 while every loop keeps up (as `/health/worker`) |
| `scrobblr_worker_loop_healthy{loop}` | 1 while the loop keeps up |
| `scrobblr_worker_loop_last_run_age_seconds{loop}` | Since its last run |
| `scrobblr_worker_loop_last_success_age_seconds{loop}` | Since its last run without an error |
| `scrobblr_worker_loop_deadline_seconds{loop}` | Interval × `WORKER_STALL_FACTOR` |
| `scrobblr_queue_due{queue}` | Items due in `classification`, `ranking`, `enrichment`, `import` |
| `scrobblr_queue_oldest_due_age_seconds{queue}` | How long the oldest due item has waited |

A Prometheus on the same host scrapes the published port:

```yaml
scrape_configs:
  - job_name: scrobblr
    scrape_interval: 60s
    authorization:
      credentials_file: /etc/prometheus/scrobblr-token   # METRICS_TOKEN
    static_configs:
      - targets: ["127.0.0.1:8080"]
```

Alerts worth having: `scrobblr_up == 0`, `scrobblr_worker_healthy == 0`
for 10 min, `rate(scrobblr_http_responses_total{class="5xx"}[5m]) > 0`,
and a sustained rise in `scrobblr_http_rate_limited_total` (clients sharing
an address because the proxy doesn't name them).

### Logs

`LOG_FORMAT=json` turns the API's and worker's log lines into one JSON
object each, for a collector; `RUST_LOG` filters them. Every service's
Docker log is rotated at 10 MB, 5 files.

## Behind a reverse proxy

The API serves plain HTTP on port 8080. Whatever proxy terminates TLS in
front of it must:

- **Name the client.** Send exactly one address in `X-Forwarded-For`, the
  client the proxy saw, replacing whatever the request came with. Every
  limit counts that address; without it, all users share the proxy's. If
  the proxy is itself behind a CDN, that address is the one the CDN
  reports, believed only from the CDN's ranges.
- **Not hold back the live stream.** `GET /v1/user/{username}/live` is a
  Server-Sent Events stream that stays open: no response buffering, no
  compression, and a read timeout longer than its 15 s keep-alive. The API
  sends `X-Accel-Buffering: no` and never compresses it itself.
- **Let uploads through.** Allow request bodies up to 8 MiB (nginx's
  default is 1 MiB).

Then tell the API which peers are that proxy (`.env`), and publish its port
where only the proxy reaches it:

| | Proxy on the same host | Proxy on another machine |
| --- | --- | --- |
| `API_BIND` | `127.0.0.1` (default) | the host's address on the network the proxy uses |
| `TRUSTED_PROXY_HOPS` | `1` | `1` |
| `TRUSTED_PROXIES` | unset: loopback and private networks, which covers Docker's bridge, where a proxy on the host connects from | the proxy's address alone |

On another machine, bind a specific address rather than `0.0.0.0`, and
check the port can't be reached from elsewhere: Docker's published ports
bypass firewalls such as ufw. A proxy running in a container on the same
host can also join the `scrobblr_default` network and reach `api:8080`.

With `TRUSTED_PROXY_HOPS=0`, the default, `X-Forwarded-For` is ignored. The
API logs which it does at startup.

Set the public URLs too: `PUBLIC_BASE_URL` (the API's public origin),
`UPLOAD_PUBLIC_URL` if images are served from elsewhere
([uploads.md](uploads.md)), `WEB_APP_URL`, and `CORS_ALLOWED_ORIGINS` only
for web pages that call the API from the browser.

An nginx example, to show the headers and buffering above; adapt it to
your host:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;   # replaces, never appends
    proxy_buffering off;
    proxy_read_timeout 1h;
    client_max_body_size 9m;
}
```
