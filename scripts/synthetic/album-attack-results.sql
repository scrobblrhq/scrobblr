-- What album attribution made of album-attack.sql, after `worker albums
-- backfill`: for each attacked track, the album it had, the accounts behind
-- each album (all of them, and those old enough to count), and the album it
-- has now.

SELECT a.attack, t.title AS track, was.title AS was, al.title AS album,
       count(*) AS accounts,
       count(*) FILTER (WHERE u.created_at <= NOW() - INTERVAL '7 days') AS counting,
       now_.title AS now
FROM synth_album_attacks a
JOIN tracks t ON t.id = a.track_id
JOIN albums was ON was.id = a.album_id
JOIN albums now_ ON now_.id = t.album_id
JOIN track_album_votes v ON v.track_id = a.track_id
JOIN albums al ON al.id = v.album_id
JOIN users u ON u.id = v.user_id
GROUP BY a.attack, t.title, was.title, al.title, now_.title
ORDER BY a.attack, accounts DESC;
