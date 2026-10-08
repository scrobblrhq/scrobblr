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
for the API and for the uploads host, with certificates from Let's Encrypt.
Its configuration is `deploy/Caddyfile`. In `.env.docker`:

```bash
API_DOMAIN=api.example.com
UPLOADS_DOMAIN=cdn.example.com
PUBLIC_BASE_URL=https://api.example.com
UPLOAD_PUBLIC_URL=https://cdn.example.com
TRUSTED_PROXY_HOPS=1
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
  takedown, purge the URL in Cloudflare.
- Leave the API's hostname unproxied (grey cloud). If you proxy it too,
  configure Caddy's `trusted_proxies` with Cloudflare's address ranges and
  set `TRUSTED_PROXY_HOPS=2`. Otherwise rate limits see Cloudflare's
  addresses instead of your users'.

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

To restore, unpack into the volume and give the files back to the API's user:

```bash
docker run --rm -v scrobblr_uploads:/uploads -v "$PWD":/backup alpine \
  sh -c 'tar xzf /backup/uploads.tar.gz -C /uploads && chown -R 10001:10001 /uploads'
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
Avatars from before aren't deleted when replaced.
