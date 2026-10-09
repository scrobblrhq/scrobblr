# Uploaded images

Avatars and community artist and album art are the only files Scrobblr
stores. Out of the box the API serves them itself, so a local or self-hosted
instance needs no setup. This guide covers moving them to their own
hostname, putting Cloudflare in front, and backups.

## How they are stored

Every upload is decoded and re-encoded as a JPEG: avatars at most 512 px a
side, artwork at most 1024 px. Nothing from the original file survives, EXIF
and GPS data included. The API accepts JPEG, PNG and WebP, detected from the
file's contents, up to 8 MiB, 12000 px a side and 40 megapixels.

The result is written under `UPLOAD_DIR` at a random key, such as
`avatars/3f/3f2a9c…e1.jpg`, `artists/…` or `albums/…`. A key is never reused
and a file never changes. The database stores the key, and clients get
`{UPLOAD_PUBLIC_URL}/{key}`. `UPLOAD_PUBLIC_URL` defaults to
`{PUBLIC_BASE_URL}/uploads`, the API's own route. Changing it moves every
image, old ones included, without touching the database.

A replaced or removed avatar is deleted. Artwork stays, since it is voted
on.

Some files can outlive their row: an avatar from before upload keys once
replaced, a delete that failed, an upload whose database write never
happened. `worker uploads gc` lists the files under `UPLOAD_DIR` that no row
refers to and are older than 24 hours (`--min-age-hours N` to change that);
`--delete` deletes them. With Compose, run it in the API container, which
has the volume:

```bash
docker compose exec api worker uploads gc
docker compose exec api worker uploads gc --delete
```

## Serving them from their own host

1. Point a DNS record such as `cdn.example.com` at the server.
2. Have a static server serve `UPLOAD_DIR` there, read-only (examples below).
3. Set `UPLOAD_PUBLIC_URL=https://cdn.example.com` for the API **and** the
   worker, and restart both.

The API keeps answering `/uploads`, so nothing breaks in between.

The static server should do what the API's route does:

- Serve keys only (paths ending in `.jpg`), never `.tmp/`, where files are
  written before being moved into place, and never directory listings.
- Send `Cache-Control: public, max-age=31536000, immutable` with every file
  it serves, but not with a 404.
- Send `X-Content-Type-Options: nosniff`, and `Access-Control-Allow-Origin: *`
  so web pages can read the pixels (for example, to draw them on a canvas).

### Docker Compose with Caddy

`docker-compose.yml` has an optional Caddy service that terminates HTTPS
for the API and for the uploads host (and the web app,
[docs/web-integration.md](web-integration.md)), with certificates from
Let's Encrypt. Its configuration is `deploy/Caddyfile`. In `.env.docker`:

```bash
API_DOMAIN=api.example.com
UPLOADS_DOMAIN=cdn.example.com
PUBLIC_BASE_URL=https://api.example.com
UPLOAD_PUBLIC_URL=https://cdn.example.com
TRUSTED_PROXY_HOPS=1
API_PORT=127.0.0.1:8080
```

```bash
docker compose --env-file .env.docker --profile caddy up -d
```

Ports 80 and 443 must reach the server. Caddy mounts the `uploads` volume
read-only.

### Caddy on the host

```caddyfile
cdn.example.com {
	root * /srv/scrobblr/uploads
	@upload {
		path_regexp ^/([a-z0-9][a-z0-9.-]*/)*[a-z0-9][a-z0-9.-]*\.jpg$
		file
	}
	handle @upload {
		header Cache-Control "public, max-age=31536000, immutable"
		header X-Content-Type-Options nosniff
		header Access-Control-Allow-Origin "*"
		file_server
	}
	handle {
		respond 404
	}
}
```

### nginx

```nginx
server {
    listen 443 ssl;
    server_name cdn.example.com;
    # ssl_certificate / ssl_certificate_key as for your other sites
    root /srv/scrobblr/uploads;

    location ~ "^/([a-z0-9][a-z0-9.-]*/)*[a-z0-9][a-z0-9.-]*\.jpg$" {
        # Without `always`, nginx adds these to successful responses only.
        add_header Cache-Control "public, max-age=31536000, immutable";
        add_header X-Content-Type-Options nosniff;
        add_header Access-Control-Allow-Origin "*";
        try_files $uri =404;
    }

    location / {
        return 404;
    }
}
```

### Permissions

The API creates files 0644 and directories 0755 whatever its umask, so a
static server running as another user can read them; only the API's user
can write. `.tmp/` is 0700. The static server's user also needs to traverse
the directories above `UPLOAD_DIR`.

A Docker named volume lives under `/var/lib/docker/volumes`, which other
users on the host can't enter. For a static server running on the host
rather than in Compose, bind-mount a host directory instead, owned by the
API container's user (uid 10001):

```yaml
    volumes:
      - /srv/scrobblr/uploads:/data/uploads
```

## Cloudflare in front

Cloudflare's free plan can cache the images at its edge, so most requests
never reach your server.

- Proxy the uploads hostname (orange cloud) and set SSL/TLS to **Full
  (strict)**. Behind the proxy, Let's Encrypt may fail to reach your server
  for renewals; a free Cloudflare origin certificate avoids that (in Caddy:
  `tls /path/to/origin.pem /path/to/origin.key` in the site block).
- Cloudflare caches `.jpg` files by default. With Browser Cache TTL set to
  respect existing headers, browsers keep the year-long `Cache-Control` too.
  New images get new URLs, so nothing ever needs purging to show up.
- A deleted image stays reachable at its old URL until Cloudflare's and
  browsers' cached copies expire. Its URL can't be guessed, but for a
  takedown, purge it (see [Taking an image down](#taking-an-image-down)).
- The API's hostname can be proxied too. `deploy/Caddyfile` trusts
  Cloudflare's address ranges and hands the API each user's address
  (`CF-Connecting-IP`), so `TRUSTED_PROXY_HOPS=1` either way. A proxy of
  your own must do the same (nginx: `set_real_ip_from` for each range,
  `real_ip_header CF-Connecting-IP`, `proxy_set_header X-Forwarded-For
  $remote_addr`), or rate limits see Cloudflare's addresses instead of your
  users'.

## Taking an image down

To remove an image someone uploaded (an avatar or artist or album art),
take the key from its URL, the part after the uploads base, such as
`avatars/3f/3f9c…e1.jpg`. Then:

1. **Remove what refers to it**, in one transaction. A displayed artwork
   is unlocked, so enrichment or another candidate can replace it:

   ```bash
   docker compose exec -T db psql -U scrobblr -d scrobblr -v key='avatars/3f/3f9c…e1.jpg' <<'SQL'
   BEGIN;
   UPDATE users SET image_url = NULL WHERE image_url = :'key';
   UPDATE artists SET image_url = NULL, image_locked = false WHERE image_url = :'key';
   UPDATE albums SET image_url = NULL, image_locked = false WHERE image_url = :'key';
   DELETE FROM image_candidates WHERE url = :'key';
   COMMIT;
   SQL
   ```

2. **Delete the file**, as the API's user:

   ```bash
   docker compose exec api rm /data/uploads/avatars/3f/3f9c…e1.jpg
   ```

3. **Purge it from Cloudflare**, or it stays served from the edge until
   its copy expires there. In the dashboard: your zone, **Caching →
   Configuration → Custom Purge**, purge by **URL**, and enter the full
   URL. Or with the API, using a token with the **Cache Purge**
   permission on the zone:

   ```bash
   curl -X POST "https://api.cloudflare.com/client/v4/zones/$ZONE_ID/purge_cache" \
     -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
     -H "Content-Type: application/json" \
     --data '{"files": ["https://uploads.example.com/avatars/3f/3f9c…e1.jpg"]}'
   ```

   Purge every URL it was served under: if the API's hostname is proxied
   too, also `https://api.example.com/uploads/<key>`. The response says
   `"success": true` once the purge is accepted; requesting the URL then
   gets a 404 from your server.

Browsers that already loaded the image keep their copy for up to a year
(`Cache-Control: immutable`), and nothing on the server can reach it. The
URL can't be guessed, so only someone who had it can see that copy.

## Backups

Back up the uploads with the database. Without them, the keys the database
holds point at nothing. Files never change once written, so incremental
tools (restic, rsync, rclone) only copy new ones, and `.tmp/` can be left
out. Dump the database **first**: files are written before a row refers to
them, so the copy that follows has every file the dump needs.

With Compose:

```bash
docker compose exec -T db pg_dump -Fc -U scrobblr scrobblr > scrobblr.dump
docker run --rm -v scrobblr_uploads:/uploads:ro -v "$PWD":/backup alpine \
  tar czf /backup/uploads.tar.gz -C /uploads .
```

To restore onto empty volumes, start only the database, load the dump the
way TimescaleDB requires, unpack the uploads and give them back to the API's
user, then start the rest:

```bash
docker compose up -d --wait db
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_pre_restore();"
docker compose exec -T db pg_restore -U scrobblr -d scrobblr < scrobblr.dump
docker compose exec -T db psql -U scrobblr -d scrobblr -c "SELECT timescaledb_post_restore();"
docker run --rm -v scrobblr_uploads:/uploads -v "$PWD":/backup alpine \
  sh -c 'tar xzf /backup/uploads.tar.gz -C /uploads && chown -R 10001:10001 /uploads'
docker compose up -d
```

## Moving to an S3-compatible bucket

Not built in yet; the code is ready for it. A bucket such as Cloudflare R2,
MinIO or Backblaze B2 would be another `Storage` variant in
`crates/api/src/media/`: write with `Content-Type: image/jpeg` and the same
`Cache-Control`, and delete. Copy the directory to the bucket under the same
keys (`rclone copy --exclude '.tmp/**' uploads/ remote:bucket`), point
`UPLOAD_PUBLIC_URL` at the bucket's public hostname, and copy again to catch
uploads made during the switch. The database doesn't change.

## Upgrading from before upload keys

Migration 0017 turns the URLs stored before, `{PUBLIC_BASE_URL}/uploads/{uuid}.jpg`,
into the key `{uuid}.jpg`. That is where those files already are, at the root
of `UPLOAD_DIR`, so they are served under `UPLOAD_PUBLIC_URL` like the rest.
Avatars from before aren't deleted when replaced; `worker uploads gc`
removes them once nothing refers to them.
