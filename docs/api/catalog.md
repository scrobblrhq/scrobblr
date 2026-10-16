# Catalog

Artists, albums and tracks, shared by everyone: created by whoever
scrobbles them first, then filled in by the server from MusicBrainz, Cover
Art Archive, Deezer and Last.fm (ids, lengths, release dates, images,
bios). Names and titles are kept as first scrobbled.

- `GET /v1/artist/{id}`, `/v1/album/{id}`, `/v1/track/{id}`: one entity by
  its id. A track lists its artist credits (`artists`: the main artist,
  then featured ones in billing order) and, for a signed-in viewer,
  whether it loves the track.
- `GET /v1/search?q=…`: artists, tracks and people at once; `type`
  `artist`, `track`, `user` or `all` (default), `limit` 10 by default and
  30 at most per kind; `q` is 1 to 200 characters. People are public
  profiles only, without accounts blocking the viewer or blocked by it.
- `GET /v1/artist/{id}/top-tracks`: its most played tracks across
  everyone (`limit` 10, at most 50).
- `GET /v1/artist/{id}/listeners`, `/v1/track/{id}/listeners`: the public
  profiles that play it most, with their play counts, as their profiles
  already show (`limit` 10, at most 50).
- **Counts**: `scrobble_count` adds up everyone's plays, private profiles'
  included, and is `null` for an entity fewer than a minimum of distinct
  accounts played (a new listener counts within the hour, today's plays
  from tomorrow). An album's count is its tracks'. `listener_count` is
  always `null`.
- **Albums**: each track counts for one album, the one the accounts
  playing it name most, whichever release a play came from; a track filed
  under no album is in no album chart.
- **Refreshing** (`POST /v1/{artist,album,track}/{id}/refresh`, `write`,
  `202`): asks the server to look the entity up again at the providers, in
  the background. An image chosen by the community is never replaced.

**Comments** on artists and tracks: `GET /v1/{artist,track}/{id}/comments`
(public, newest first, without accounts blocking the signed-in viewer or
blocked by it), `POST` with `{ "body": "…" }` (`write`, 1 to 2,000
characters, `201`), and `DELETE /v1/comments/{id}` for your own (`204`;
`404` for anyone else's). A comment shows its author's name whatever the
profile's privacy.
