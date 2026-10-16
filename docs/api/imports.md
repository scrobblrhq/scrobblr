# Imports

Users bring their Last.fm history over: loved tracks first, then every
scrobble. The server runs the import in the background.

1. **Link the Last.fm account** (**Connected accounts** section), which
   proves the user owns it.
2. **Start** with `POST /v1/import/lastfm` (`write`; a body
   `{ "full": true }` is optional): `202` with the import. `400` without a
   linked Last.fm account, `409` while another import runs, `429` within a
   day of the last one, `503` when the server has no Last.fm key.
3. **Follow** it with `GET /v1/imports/{id}` (or the 20 latest,
   `GET /v1/imports`; `read`) until `status` is `done`, `failed` or
   `cancelled`. `DELETE /v1/imports/{id}` (`write`) stops it after the page
   in flight; what was imported stays.

```json
{
  "id": 42, "provider": "lastfm", "external_user": "ana_fm", "verified": true,
  "status": "running", "error_code": null, "error_message": null,
  "window_from": null, "window_to": "2026-10-13T10:00:00Z",
  "total_expected": 31921, "fetched": 12000, "imported": 11890,
  "duplicates": 104, "skipped": 6, "oldest_played_at": "2024-02-11T21:04:00Z",
  "loved_total": 132, "loved_imported": 130, "loved_done": true, "loved_error": null,
  "retrying_at": null, "created_at": "2026-10-13T10:00:00Z",
  "started_at": "2026-10-13T10:00:05Z", "finished_at": null
}
```

- **Progress**: `fetched` against `total_expected` (known after the first
  page), `imported` new, `duplicates` already here (an earlier import, or
  the same listen scrobbled live), `skipped` invalid entries,
  `oldest_played_at` how far back it has reached. An import resumes after
  any interruption; `retrying_at` is set while it waits out a Last.fm
  error.
- **Again later**: a new import reads only what came after the last
  completed one (two weeks before its end, for late scrobbles); `full`
  reads everything. Neither creates duplicates.
- **Failures** (`error_code`): `user_not_found`, `history_hidden` (the
  Last.fm profile hides its listening and the account wasn't linked),
  `cap_reached` (the server's limit of imported scrobbles),
  `lastfm_unavailable` (Last.fm kept failing), `lastfm_error`.
- **Loved tracks** come first and add to the user's loves (up to 50,000);
  a love taken back here stays taken back on a later import, unless
  `full`. `loved_error` says why they stopped short, and the scrobbles go
  on.
- **Imported plays** show on the profile and in its charts like any other,
  and don't count in global rankings.
