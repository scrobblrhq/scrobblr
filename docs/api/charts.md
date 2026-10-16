# Charts

**A user's charts** (`GET /v1/user/{username}/charts/{artists,tracks,albums}`,
public, as visible as the profile): the account's most played artists,
tracks or albums over a period.

| parameter | | |
| --- | --- | --- |
| `period` | `7d` (default), `30d`, `90d`, `12m` (365 days), `all` | UTC days ending today, today included |
| `from`, `to` | `YYYY-MM-DD`, both included | instead of `period`: both or neither, `to` not after today, 366 days at most |
| `limit`, `offset` | 50 by default, 200 at most | |

```json
{
  "period": "30d", "from": "2026-09-14", "to": "2026-10-13",
  "total": 212, "offset": 0, "limit": 2,
  "entries": [
    { "album_id": 31, "album_title": "Mezzanine", "artist_id": 97,
      "artist_name": "Massive Attack", "image_url": null,
      "play_count": 41, "unverified_count": 3 },
    { "album_id": 8, "album_title": "Dummy", "artist_id": 12,
      "artist_name": "Portishead", "image_url": null,
      "play_count": 30, "unverified_count": 0 }
  ]
}
```

`period` is `null` for a custom range, `from` is `null` for `all`. Plays
are the profile's: duplicates left out, plays the server couldn't verify
in, counted again in `unverified_count`. A track counts for the one album
the catalog files it under, whichever release it was played from. A page
can lag the newest plays by up to an hour; send its `ETag` back to get a
`304` while it hasn't changed.

The older `GET /v1/user/{username}/top-artists` and `/top-tracks` take
`period` as `7days`, `1month`, `3months`, `6months`, `1year` or `overall`
and a `limit`.

**Community charts** (`GET /v1/user/me/community/{artists,tracks,albums}`,
`read`): what the accounts you follow play, ranked as the global rankings
are (**Rankings** section): by listeners, then plays, both whole numbers
and weighted towards real listening. The same parameters, but no `all`:
a range starts within the last year.

```json
{
  "period": "7d", "from": "2026-10-07", "to": "2026-10-13",
  "following": 64, "accounts": 64, "min_listeners": 5,
  "total": 18, "offset": 0, "limit": 50,
  "entries": [
    { "position": 1, "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_image": null, "listeners": 9, "plays": 14 }
  ]
}
```

- `following` is how many of the accounts you follow the chart may cover
  (public, not waiting to be deleted); `accounts` how many it covers: the
  most recently active, every one over a week, fewer over longer ranges
  (40 over a year).
- An entry needs `min_listeners` of those accounts: following fewer
  accounts than that gives an empty chart. Nothing names an account.
- A chart is kept for 5 minutes, and pages and repeat views read it;
  following or unfollowing someone makes a new one.
