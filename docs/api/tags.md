# Tags

Users tag artists, albums and tracks, as on Last.fm. What a user tagged is
theirs and shows with their profile; what everyone sees on an entity is
the tags enough established accounts put there.

| route | access | |
| --- | --- | --- |
| `GET /v1/{artist,album,track}/{id}/tags` | public | the tags shown on it (`TopTags`), most taggers first |
| `GET /v1/tag/{tag}/{artists,albums,tracks}` | public | the entities a tag is shown on (`TaggedEntityPage`) |
| `POST /v1/{artist,album,track}/{id}/tags` | `write` | `{ "tags": ["trip-hop"] }`: add up to 10 of yours; answers with your tags on it (`EntityTags`) |
| `DELETE /v1/{artist,album,track}/{id}/tags/{tag}` | `write` | remove one of yours: `204`, also when it wasn't there |
| `GET /v1/user/me/tags` | `read` | your tags, newest first (`UserTagPage`); `?artist=`, `album=` or `track=` for one entity |
| `GET /v1/user/{username}/tags` | public, as visible as the profile | the tags a user uses, and on how many entities (`UserTags`) |

```json
{ "entity_type": "artist", "entity_id": 97,
  "tags": [ { "name": "trip-hop", "taggers": 41 }, { "name": "electronic", "taggers": 12 } ] }
```

- **Names** are stored in one form: Unicode compatibility forms folded
  (full-width letters, ligatures), lowercase, single spaces, trimmed; 1 to
  40 characters of letters, digits, spaces and `- & ' . + ! _`, with at
  least one letter or digit. Anything else is a `400`, and so is a tag the
  server refuses. Paths take any spelling: `/v1/tag/Trip%20%20Hop/artists`
  is `trip hop`.
- **Caps**: 10 of your tags on one entity, 500 different tags on one
  entity, 50,000 taggings per account (`409`). An account that can't tag
  for now gets a `403`.
- **What everyone sees**: a tag shows on an entity once enough
  established accounts use it there; `taggers` is about how many, private
  accounts counted unnamed. New tags show within a minute or so; a tag
  below the minimum doesn't show at all, so your own tags on an entity
  (`GET /v1/user/me/tags?artist=…`) can be more than everyone sees.
- **Pages**: a tag's entities page by `limit` (50, at most 200) and
  `offset`, with `total`; your tags by cursor (`before`).
- **Caching**: both public reads send `Cache-Control: public, max-age=300`
  and an `ETag`.

Tags aren't imported from Last.fm. Scrobbler apps tag through the Last.fm
protocol (**Scrobbler protocols** section), within the same caps and
limits.
