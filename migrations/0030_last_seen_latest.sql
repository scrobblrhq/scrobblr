-- =============================================================
--  Scrobblr — users.last_seen_at is the latest play
--
--  The counter trigger set last_seen_at to each new play's time, so a
--  late play (a phone's offline queue, a retried batch) moved it back.
--  The follow feed skips followed accounts with no play since a page's
--  window began by this column, which needs it to be the latest: the
--  trigger keeps the greater, as imports already did, and accounts it
--  moved back get their latest play again.
-- =============================================================

CREATE OR REPLACE FUNCTION increment_scrobble_counts()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.import_id IS NOT NULL THEN
        RETURN NEW;
    END IF;

    UPDATE tracks  SET scrobble_count = scrobble_count + 1 WHERE id = NEW.track_id;
    UPDATE artists SET scrobble_count = scrobble_count + 1 WHERE id = NEW.artist_id;
    UPDATE users   SET scrobble_count = scrobble_count + 1,
                       last_seen_at   = GREATEST(last_seen_at, NEW.played_at)
    WHERE id = NEW.user_id;

    IF NEW.album_id IS NOT NULL THEN
        UPDATE albums SET scrobble_count = scrobble_count + 1 WHERE id = NEW.album_id;
    END IF;

    RETURN NEW;
END;
$$;

-- One index probe per account (the newest chunk holding its plays).
UPDATE users u
SET last_seen_at = l.played_at
FROM (
    SELECT u2.id,
           (SELECT s.played_at FROM scrobbles s WHERE s.user_id = u2.id
            ORDER BY s.played_at DESC LIMIT 1) AS played_at
    FROM users u2
) l
WHERE l.id = u.id AND l.played_at > COALESCE(u.last_seen_at, '-infinity');
