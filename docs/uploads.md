# Uploaded images

Avatars and community artist and album art are the only files Scrobblr
stores. The API serves them itself, so an instance needs no setup for them.

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
image, old ones included, without touching the database. Images uploaded
before migration 0017 have keys of the form `{uuid}.jpg`, at the root of
`UPLOAD_DIR`.

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

## Serving

The API answers `/uploads/{key}` with keys only (never `.tmp/`, where files
are written before being moved into place, and never directory listings),
`Cache-Control: public, max-age=31536000, immutable`,
`X-Content-Type-Options: nosniff` and `Access-Control-Allow-Origin: *`, so
web pages can read the pixels. Another server can serve the directory
instead: point `UPLOAD_PUBLIC_URL` at it, for the API **and** the worker,
and have it send the same headers.

## Permissions

The API creates files 0644 and directories 0755 whatever its umask, so a
server running as another user can read them; only the API's user (uid
10001 in the image) can write. `.tmp/` is 0700. A restored copy must be
owned by that user again.

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

3. **Purge it from any cache in front** (a CDN or a caching proxy), or it
   stays served from there until its copy expires.

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
