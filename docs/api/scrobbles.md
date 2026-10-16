# Scrobbles

A user's listening, as visible as the profile: public, or the owner's
alone when private.

- **Recent scrobbles** (`GET /v1/user/{username}/recent`): newest first,
  `limit` 50 by default and 200 at most; for older ones pass the last
  `played_at` seen as `before`. Each carries `status` (as in the
  **Scrobbling** section) and `loved`, whether the user loves the track
  now. Duplicates are left out.
- **Top artists and tracks** (`/top-artists`, `/top-tracks`): the most
  played over a `period` (`limit` 10 by default, 50 at most); the
  **Charts** section has the paged, ranged version of the same lists.
- **Heatmap** (`/heatmap`): plays per UTC day over the last 365 days,
  oldest first, days without plays left out.
- **Live** (`/live`): a Server-Sent Events stream of what the user plays.
  The first event is the current state (`null` when nothing plays), then
  one per update, each a `NowPlayingRich`. An entry that ends sends no
  event: compare its `expires_at` with the clock. An idle stream sends a
  comment every 15 seconds. A block closes the blocked account's streams
  of the blocker, and a profile turning private closes everyone's but its
  owner's.

```text
GET /v1/user/ana/live
Accept: text/event-stream

data: null

data: {"track_id":1841,"track_title":"Teardrop","artist_name":"Massive Attack","album_title":"Mezzanine","album_image":null,"artist_image":null,"started_at":"2026-10-13T10:41:07Z","expires_at":"2026-10-13T10:46:37Z","source":"my-player"}
```
