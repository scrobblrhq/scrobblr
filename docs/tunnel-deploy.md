# Running behind a Cloudflare Tunnel

`docker-compose.tunnel.yml` puts the stack behind the tunnel `scrobblr`
(id `49ca94d1-cfc8-4c1a-a233-4db6a9da97ec`): cloudflared connects out to
Cloudflare, which terminates TLS for `scrobblr.app`, `api.scrobblr.app` and
`cdn.scrobblr.app`, and hands every request to Caddy over plain HTTP. No
port is published on the host. Caddy believes `CF-Connecting-IP` from
cloudflared alone and passes that one address on in `X-Forwarded-For`.
The live now-playing stream sends a comment every 15 s, well within
Cloudflare's 100 s idle limit. `deploy/tunnel-test/run.sh` checks all of it
with a stand-in for Cloudflare.

## The host

- Docker Engine with Compose 2.24 or newer (`docker compose version`), and
  git. Docker starts at boot: `sudo systemctl enable docker containerd`.
- No sleep: `sudo systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target`
  (on a laptop, also `HandleLidSwitch=ignore` in `/etc/systemd/logind.conf`).
- Clock synchronized: `timedatectl` says `System clock synchronized: yes`,
  else `sudo timedatectl set-ntp true`. Audioscrobbler clients are refused
  beyond 5 minutes of skew.
- Firewall: nothing comes in except SSH (`sudo ufw default deny incoming`,
  `sudo ufw allow OpenSSH`, `sudo ufw enable`). The tunnel goes out on 443
  and 7844, TCP and UDP.

## Setting up

```bash
git clone https://github.com/scrobblrhq/scrobblr.git ~/scrobblr && cd ~/scrobblr
```

Copy the tunnel's credentials file from `~/.cloudflared/` on your machine to
`deploy/cloudflared/49ca94d1-cfc8-4c1a-a233-4db6a9da97ec.json` (git-ignored,
like `cert.pem`, which stays on your machine), readable by cloudflared's
user only:

```bash
sudo chown 65532:65532 deploy/cloudflared/49ca94d1-cfc8-4c1a-a233-4db6a9da97ec.json
sudo chmod 400 deploy/cloudflared/49ca94d1-cfc8-4c1a-a233-4db6a9da97ec.json
```

`cp .env.docker.example .env.docker`, `chmod 600 .env.docker`, then set in
it (passwords from `openssl rand -hex 32`; keep a copy off the machine):

```bash
POSTGRES_PASSWORD=…
REDIS_PASSWORD=…
COMPOSE_FILE=docker-compose.yml:docker-compose.tunnel.yml
COMPOSE_PROFILES=caddy
WEB_DOMAIN=scrobblr.app
API_DOMAIN=api.scrobblr.app
UPLOADS_DOMAIN=cdn.scrobblr.app
```

The API's URLs follow from the three domains. Without the web app,
`scrobblr.app` answers 503 and the API and uploads work; once its image
exists, add `WEB_IMAGE=…` and make it `COMPOSE_PROFILES=caddy,web`.

## Running it

In `~/scrobblr`:

```bash
docker compose --env-file .env.docker up -d --build            # start, or apply changes
docker compose --env-file .env.docker ps                       # cloudflared turns healthy once connected
docker compose --env-file .env.docker logs -f --tail 100 api   # or cloudflared, caddy, worker…
docker compose --env-file .env.docker down                     # stop; never -v, which deletes the data
git pull && docker compose --env-file .env.docker up -d --build   # update
```

## Backups

From your machine, database first (a file is written before a row refers to
it). Volumes are named after the project, `scrobblr`.

```bash
H=you@her-machine; C="cd scrobblr && docker compose --env-file .env.docker"
ssh "$H" "$C exec -T db pg_dump -Fc -U scrobblr scrobblr" > scrobblr.dump
ssh "$H" "$C exec -T api tar -C /data/uploads --exclude=./.tmp -cf - ." > uploads.tar
```

`pg_dump` warns about circular foreign keys on `continuous_agg`; that dump
restores fine. To restore, onto empty volumes (this deletes the current
data):

```bash
ssh "$H" "$C down && docker volume rm scrobblr_db-data scrobblr_uploads"
ssh "$H" "$C up -d --wait db"
ssh "$H" "$C exec -T db psql -U scrobblr -d scrobblr -c 'SELECT timescaledb_pre_restore();'"
ssh "$H" "$C exec -T db pg_restore -U scrobblr -d scrobblr" < scrobblr.dump
ssh "$H" "$C exec -T db psql -U scrobblr -d scrobblr -c 'SELECT timescaledb_post_restore();'"
ssh "$H" "$C run --rm --no-deps -T api tar -C /data/uploads -xf -" < uploads.tar
ssh "$H" "$C up -d --build"
```

## Revoking the tunnel

From your machine (with `cert.pem`); her machine stops receiving traffic at
once and the credentials file becomes useless:

```bash
cloudflared tunnel cleanup scrobblr
cloudflared tunnel delete scrobblr
```

Then delete or repoint the three DNS records, which still name the tunnel.
