# Rankings

The server's global charts of artists and tracks (`GET /v1/rankings/artists`,
`GET /v1/rankings/tracks`), public, the same for everyone.

| parameter | | |
| --- | --- | --- |
| `period` | `week` (default), `month`, `year` | the last 7, 30 or 365 UTC days, today included; anything else is a `400` |
| `limit`, `offset` | 50 by default, 200 at most | positions to skip; past the end, an empty page |

```json
{
  "period": "week", "from": "2026-10-04", "to": "2026-10-10",
  "computed_at": "2026-10-10T22:01:07.512Z", "stale": false,
  "total": 812, "offset": 0, "limit": 2,
  "entries": [
    { "position": 1, "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_image": null,
      "listeners": 412, "plays": 1290 },
    { "position": 2, "track_id": 77, "track_title": "Windowlicker", "artist_id": 12,
      "artist_name": "Aphex Twin", "album_image": null, "listeners": 398, "plays": 1517 }
  ]
}
```

- **Order**: by `listeners`, then `plays`, both whole numbers. They count
  real listening: plays that don't look like a person listening count
  less or not at all, and imported history doesn't count. They are not a
  sum of profiles' play counts, and no raw counts are served.
- **Who is in it**: every account, private ones included, never named.
  An artist or track with too few listeners is left out, so no position
  stands for one or two people.
- **Freshness**: rankings are computed in the background, the week's and
  month's every hour, the year's every day, and when the UTC day changes.
  `computed_at` says when, `from` and `to` which days. A refresh running
  late is served anyway with `stale: true`; before the first one, the
  answer is `404`. A page reads one stored ranking, so pages fetched across
  a refresh can repeat or skip a position (`computed_at` changes).
- **Caching**: `Cache-Control: public, max-age=300,
  stale-while-revalidate=300` and an `ETag`; `If-None-Match` gets a `304`.

Artists' entries carry `artist_id`, `artist_name` and `image_url`; tracks'
the track, its artist, and the album cover as `album_image`.
