# Recommendations

What listening says about artists and people: an artist's similar
artists, a user's taste neighbours (accounts that chose to be shown whose
listening is closest), and artists recommended to the signed-in user. The
server computes all three once a day and serves what it stored, minus what
changed since (plays, skips, blocks, privacy).

| route | access | |
| --- | --- | --- |
| `GET /v1/artist/{id}/similar` | public | a page; `404` no such artist |
| `GET /v1/user/{username}/similar` | public, as visible as the profile | the taste neighbours; `404` unless the user shows them |
| `GET /v1/user/me/recommendations/artists` | `read` | a page |
| `POST /v1/user/me/recommendations/artists/{id}/skip` | `write` | hide an artist for 90 days: `204`; `404` no such artist |
| `DELETE /v1/user/me/recommendations/artists/{id}/skip` | `write` | show it again: `204` |
| `GET /v1/user/me/similarity` | `read` | the setting |
| `PUT /v1/user/me/similarity` | session | `{ "mode": "listed" }`; `400` an unknown mode |

**Similar artists** (`limit` 20, at most 50; `offset`): whose listeners
also play the artist, `score` from 0 to 1. `common_listeners` is a lower
bound (1, 2, 5, 10, 20, 50, 100, …), never an exact count. An artist with
too few listeners has none (`total: 0`); before the first daily run
`computed_at` is `null`. `stale` is `true` when the daily run is late (the
last results are still served). Cacheable for an hour, with an `ETag`.

```json
{
  "artist_id": 97, "computed_at": "2026-10-14T03:12:40Z", "stale": false,
  "total": 50, "offset": 0, "limit": 1,
  "entries": [
    { "position": 1, "artist_id": 12, "artist_name": "Portishead",
      "image_url": null, "score": 0.412, "common_listeners": 200 }
  ]
}
```

**Recommendations** (`limit` 20, at most 100; `offset`): artists the user
never played, ranked by how much the listeners of the user's artists play
them; `score` is relative to the first (1.0). `because` is the user's own
artist that counted most, `because_count` how many of the user's artists
led there. Artists played since the daily run, or skipped, are left out
at once. Imported history counts here, for the user's own results. Send
the `ETag` back to get a `304` while nothing changed.

```json
{
  "status": "ready", "computed_at": "2026-10-14T03:12:40Z", "stale": false,
  "total": 87, "offset": 0, "limit": 20,
  "entries": [
    { "position": 1, "artist_id": 31, "artist_name": "Tricky", "image_url": null,
      "score": 1.0, "because": { "artist_id": 97, "artist_name": "Massive Attack" },
      "because_count": 3 }
  ]
}
```

**Taste neighbours** (`limit` 10, at most 30): accounts that chose
`listed`, public, not blocking or blocked by the list's owner or the
viewer, with a `score`, how many artists they share and the three
counting most (`top_shared`). A user's own list is theirs whatever the
setting; someone else's is shown only when that user is `listed` (a `404`
otherwise, as for a missing user).

`status` (recommendations and neighbours): `ready`; `none`, too little
listening yet, or nobody close enough; `off`, the user turned similarity
off; `pending`, nothing computed yet.

**The setting** (`mode`):

| mode | the account's listening | shown as a taste neighbour | its own results |
| --- | --- | --- | --- |
| `listed` | counts, anonymously, in similar artists and others' recommendations | yes, while the profile is public | yes, and anyone may see its neighbours |
| `anonymous` (default) | counts, anonymously | never | yes, its own to see |
| `off` | not used | never | none |

`GET` answers `{ "mode": "anonymous", "shown": false, "changed_at": null }`
(`shown`: others see the account as a neighbour now). Leaving `listed`
removes the account from others' lists at once; `off` also deletes its own
results, and its anonymous part leaves similar artists at the next daily
run.
