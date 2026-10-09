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

## Backup and restore

Back up the database and the uploads together: the database holds the
uploads' keys. Dump the database **first**: an image is written before a
row refers to it, so the copy that follows has every file the dump needs.
`scrobblr` below is `POSTGRES_USER` and `POSTGRES_DB`.

```bash
docker compose exec -T db pg_dump -Fc -U scrobblr scrobblr > scrobblr.dump
docker run --rm -v scrobblr_uploads:/uploads:ro -v "$PWD":/backup alpine \
  tar czf /backup/uploads.tar.gz -C /uploads .
```

Uploaded files never change, so incremental tools (restic, rsync) copy only
new ones; `.tmp/` can be left out.

To restore onto empty volumes (a new host, or after removing the old
volumes on purpose), start only the database, load the dump between
TimescaleDB's `timescaledb_pre_restore()` and `timescaledb_post_restore()`,
unpack the uploads and give them back to the API's user (uid 10001), then
start the rest:

```bash
docker compose up -d --wait db
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_pre_restore();"
docker compose exec -T db pg_restore -U scrobblr -d scrobblr --no-owner < scrobblr.dump
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_post_restore();"
docker run --rm -v scrobblr_uploads:/uploads -v "$PWD":/backup alpine \
  sh -c 'tar xzf /backup/uploads.tar.gz -C /uploads && chown -R 10001:10001 /uploads'
docker compose up -d
```

`pg_dump` warns about circular foreign keys on `continuous_agg`; that is
TimescaleDB's catalog and harmless with a full dump. The dump must come from
the same TimescaleDB version as the image, the one `docker-compose.yml`
pins.

`TOKEN_ENCRYPTION_KEY` is not in the dump: keep it apart from the backups,
and restore it with them, or linked Spotify and Last.fm accounts must be
connected again.

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
