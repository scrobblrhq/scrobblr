-- =============================================================
--  Scrobblr — Upload keys
--  Image columns hold an upload's key (avatars/3f/3f…e1.jpg) rather
--  than its URL, and the API serves keys under UPLOAD_PUBLIC_URL.
--  Uploads made before this were stored as
--  {PUBLIC_BASE_URL}/uploads/{uuid}.jpg for the file {uuid}.jpg at
--  the root of UPLOAD_DIR, which is their key. Provider URLs and
--  anything else stay as they are.
-- =============================================================

UPDATE users
SET image_url = substring(image_url FROM '/uploads/([0-9a-f-]{36}\.jpg)$')
WHERE image_url ~ '^https?://[^?#]*/uploads/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.jpg$';

UPDATE artists
SET image_url = substring(image_url FROM '/uploads/([0-9a-f-]{36}\.jpg)$')
WHERE image_url ~ '^https?://[^?#]*/uploads/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.jpg$';

UPDATE albums
SET image_url = substring(image_url FROM '/uploads/([0-9a-f-]{36}\.jpg)$')
WHERE image_url ~ '^https?://[^?#]*/uploads/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.jpg$';

UPDATE image_candidates
SET url = substring(url FROM '/uploads/([0-9a-f-]{36}\.jpg)$')
WHERE url ~ '^https?://[^?#]*/uploads/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.jpg$';
