# Loved tracks

A user can love a track, as on Last.fm: a list on the profile and a heart
on the track's page, in recent scrobbles and in the feed. Loves are what a
user says, not listening: they show on the profile and count nowhere else
(no rankings, recommendations or "loved by" numbers).

| route | access | |
| --- | --- | --- |
| `POST /v1/track/{id}/love` | `write` | `204`, also when loved already (the first `loved_at` stands); `404` not in the catalog; `409` at 50,000 loved tracks |
| `DELETE /v1/track/{id}/love` | `write` | `204`, also when not loved; `404` not in the catalog |
| `GET /v1/user/{username}/loved` | public, as visible as the profile | a page, newest love first |
| `GET /v1/user/me/loved` | `read` | the same, whatever the profile's privacy |

```json
{
  "entries": [
    { "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_id": 31, "album_title": "Mezzanine",
      "album_image": null, "artist_image": null,
      "loved_at": "2026-10-13T10:41:07.123456Z" }
  ],
  "total": 132,
  "next": "1760352067123456.1841"
}
```

Pages go by cursor (`limit` 50, at most 200; `before`); `total` counts
every loved track. `GET /v1/track/{id}` says whether the viewer loves it
(`loved`, `null` signed out); recent scrobbles and feed entries say
whether the account that played the track loves it now. Now playing
carries `track_id`, so an app can love what is playing.

The native routes love by track id and create nothing. Scrobbler apps
love by artist and title through the Last.fm and ListenBrainz protocols,
and a Last.fm import brings the account's loved tracks over (the
**Scrobbler protocols** and **Imports** sections).
