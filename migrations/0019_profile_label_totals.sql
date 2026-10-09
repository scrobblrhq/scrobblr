-- =============================================================
--  Scrobblr — Profile stats leave duplicates out
--
--  A profile shows its user's listening, suspect plays included, but never
--  the classifier's duplicates (the same listen reported again).
--  users.scrobble_count now leaves classified duplicates out, and
--  user_label_totals sums each user's classified days, so the profile can
--  tell verified plays from unverified ones. A trigger on
--  scrobble_classification_days keeps both, whatever writes the days.
-- =============================================================

CREATE TABLE user_label_totals (
    user_id     BIGINT  PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- Scrobbles in classified days, as they were labelled.
    classified  BIGINT  NOT NULL DEFAULT 0,
    counted     BIGINT  NOT NULL DEFAULT 0,
    suspect     BIGINT  NOT NULL DEFAULT 0,
    duplicate   BIGINT  NOT NULL DEFAULT 0,
    no_data     BIGINT  NOT NULL DEFAULT 0
);

CREATE OR REPLACE FUNCTION track_user_label_totals()
    RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    d            scrobble_classification_days := CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    sign         INT    := CASE WHEN TG_OP = 'DELETE' THEN -1 ELSE 1 END;
    d_classified BIGINT := sign * d.scrobble_count;
    d_counted    BIGINT := sign * d.counted;
    d_suspect    BIGINT := sign * d.suspect;
    d_duplicate  BIGINT := sign * d.duplicate;
    d_no_data    BIGINT := sign * d.no_data;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        d_classified := d_classified - OLD.scrobble_count;
        d_counted    := d_counted - OLD.counted;
        d_suspect    := d_suspect - OLD.suspect;
        d_duplicate  := d_duplicate - OLD.duplicate;
        d_no_data    := d_no_data - OLD.no_data;
    END IF;

    -- A deleted user's days go with it: never insert a row for them.
    IF TG_OP = 'DELETE' THEN
        UPDATE user_label_totals
        SET classified = classified + d_classified,
            counted    = counted + d_counted,
            suspect    = suspect + d_suspect,
            duplicate  = duplicate + d_duplicate,
            no_data    = no_data + d_no_data
        WHERE user_id = d.user_id;
    ELSE
        INSERT INTO user_label_totals AS t (user_id, classified, counted, suspect, duplicate, no_data)
        VALUES (d.user_id, d_classified, d_counted, d_suspect, d_duplicate, d_no_data)
        ON CONFLICT (user_id) DO UPDATE
            SET classified = t.classified + EXCLUDED.classified,
                counted    = t.counted + EXCLUDED.counted,
                suspect    = t.suspect + EXCLUDED.suspect,
                duplicate  = t.duplicate + EXCLUDED.duplicate,
                no_data    = t.no_data + EXCLUDED.no_data;
    END IF;

    IF d_duplicate <> 0 THEN
        UPDATE users SET scrobble_count = scrobble_count - d_duplicate WHERE id = d.user_id;
    END IF;
    RETURN NULL;
END;
$$;

INSERT INTO user_label_totals (user_id, classified, counted, suspect, duplicate, no_data)
SELECT user_id, sum(scrobble_count), sum(counted), sum(suspect), sum(duplicate), sum(no_data)
FROM scrobble_classification_days
GROUP BY user_id;

UPDATE users u
SET scrobble_count = u.scrobble_count - t.duplicate
FROM user_label_totals t
WHERE t.user_id = u.id AND t.duplicate <> 0;

CREATE TRIGGER trg_user_label_totals
    AFTER INSERT OR UPDATE OR DELETE ON scrobble_classification_days
    FOR EACH ROW EXECUTE FUNCTION track_user_label_totals();
