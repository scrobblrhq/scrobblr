# Scrobbling

A scrobble is one listen: what played and when it started. Players send
them with a session or a token with the `scrobble` scope, the only scope a
player needs.

```http
POST /v1/scrobble HTTP/1.1
Authorization: Bearer <token>
Content-Type: application/json

{
  "track": "Teardrop",
  "artist": "Massive Attack",
  "album": "Mezzanine",
  "played_at": "2026-10-13T10:41:07Z",
  "duration_ms": 330000,
  "listened_ms": 165000,
  "source": "my-player"
}
```

The answer is `201` with the scrobble's `id` and `played_at`.

- **When to scrobble**: once the track has played for half its length or
  4 minutes, whichever comes first, and at least 30 seconds, as Last.fm
  clients do. A play reporting `listened_ms` under both 30 seconds and half
  of `duration_ms` is refused.
- **What to send**: `track` and `artist` (at most 300 characters each, as
  `album`), `played_at` when the track started, and whatever else is known:
  `duration_ms` (lengths outside 1 ms to 24 hours count as unknown),
  `listened_ms`, `featured_artists` (up to 16, in billing order; the main
  artist is `artist`), `source` (the player's name, cut to 100 characters)
  and `recording_mbid`, a MusicBrainz recording id the server checks
  against the title and artist before using it. Lengths and listened time
  help the server check plays.
- **Refused with `422`**: a play more than 30 days old (bring older
  history over with a Last.fm import), one more than 5 minutes in the
  future, an empty title or artist, a listen too short, and a duplicate:
  the same track within 30 seconds of a scrobble already stored, which
  is how retries and several players reporting one listen look.
  Duplicates are safe to send; a client should drop a play the server
  refused with any `4xx` but `429`.
- **The daily allowance**: each account records a number of scrobbles per
  UTC day (3,000 by default), counted across every API; past it, `429`
  until midnight UTC. Refused plays don't count.

**Now playing** (`POST /v1/now-playing`, `204`): the track starting now,
with the same fields but `played_at` and `listened_ms`. It shows on the
profile and its live stream until the track would end (5 minutes without
a length), or until the next update.

**Afterwards** every play is checked against the account's other plays.
Recent scrobbles carry the outcome as `status`: `counted`; `suspect`, more
listening than the time allows (several players at once, a script);
`no_data`, a track whose length nobody knows yet; `null`, not checked yet,
for a minute or so. Profiles count every play but the duplicates the check
finds; `scrobble_breakdown` on a profile says how many are verified.
