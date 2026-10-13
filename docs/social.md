# Social: follows, charts and the feed

What users see of each other: follows, a user's own charts, the charts of
the accounts a user follows (community charts), and a feed of what those
accounts play. Global rankings are in [rankings.md](rankings.md); what is
stored and erased about an account, in [privacy.md](privacy.md).

## Who sees whom

One rule, applied everywhere an account's data or name is shown: an
account is **visible** to a viewer when it isn't waiting to be deleted
and it is public, or it is the viewer's own.

- **A private profile is its owner's alone.** Its profile, follow lists,
  recent scrobbles, top lists, charts, heatmap and live stream answer 403 to
  everyone else, followers included (`middleware/visibility.rs`). Following
  takes no approval, so a follower is anyone who clicked follow; until
  2026-10-12 followers could see a private profile, which let anyone see
  any. The Last.fm-compatible API already showed a private profile to its
  owner only.
- **Lists of people** (followers, following, listeners of an artist or a
  track, search) leave out private accounts, except the viewer's own entry,
  and accounts waiting to be deleted (404 on their own routes).
- **Community charts and the feed** cover only visible followed accounts:
  following a private account adds nothing to either.
- **Counts** (`followers_count`, `following_count`, catalog counts, global
  rankings) include private accounts, unnamed, as every aggregate does.
- Comments and uploaded images keep their author's name whatever the
  privacy setting: they were posted for others to see.
- **Blocks** ([below](#blocking-and-removing-followers)): an account that
  blocks the viewer doesn't exist for it, and neither account appears in
  the other's lists of people.

## Follows

| route | access | |
| --- | --- | --- |
| `POST /v1/user/{username}/follow` | `write` | 204, also when already following; 400 yourself, 404 missing or leaving, 409 at the cap, 429 past the rate |
| `DELETE /v1/user/{username}/follow` | `write` | 204; free |
| `GET /v1/user/{username}/followers` | public | a page, newest follow first |
| `GET /v1/user/{username}/following` | public | the same |
| `GET /v1/user/{username}/friends` | public | the 100 most recent of each list (it returned them whole) |

- **Pages** (`FollowPage`): `limit` (50, at most 200) and `before` (the
  previous page's `next`, an opaque keyset cursor of the follow's time and
  the account's id; `null` on the last page). Entries are `UserProfile`s
  with `followed_at`, and for a signed-in viewer `is_following` (the viewer
  follows them) and `follows_you` (they follow the viewer): mutual follows
  are both. Hidden accounts are skipped, so a page can hold fewer entries
  than `limit` and still have a next one. Migration `0028` indexes both
  directions by (account, follow time, other account).
- **Profiles** (`GET /v1/user/{username}`, `/v1/user/me`) add
  `followers_count`, `following_count` (every follow, counted from the
  index) and `follows_you`.
- **Limits:** 2,000 accounts followed at most (`MAX_FOLLOWING`, 409 past
  it, serialized per follower); 100 follows an hour per account and 300 per
  address (`limits::follow_attempt`, refused ones included, Redis, fails
  open like uploads). Unfollowing is free; following again is no new
  follow but counts as an attempt.

## Blocking and removing followers

| route | access | |
| --- | --- | --- |
| `POST /v1/user/{username}/block` | session | 204, also when already blocking; 400 yourself, 404 missing, leaving or blocking you, 409 at the cap, 429 past a rate |
| `DELETE /v1/user/{username}/block` | session | 204, also when not blocking; 404 as above unless you block it, 429 |
| `GET /v1/user/me/blocks` | session | a page, newest block first (`BlockPage`); 400 bad cursor |
| `DELETE /v1/user/me/followers/{username}` | session | 204, also when it doesn't follow you; 400 yourself, 404 as above, 429 |

**A block** (`user_blocks`, migration `0031`) ends the follows between the
two accounts, either way, and neither can follow the other while it stands
(the blocked one gets a 404, the blocker a 409 saying it blocks the
account). Then:

- **The blocked account finds no such account**, signed in: the
  blocker's profile and every route under it (follow lists, recent plays,
  top lists, charts, heatmap, live stream) answer 404 with the body a
  missing account gets, a private profile too (rather than its usual 403),
  and so do following, unfollowing, blocking, unblocking and removing it,
  and the Last.fm-compatible `user.getInfo` and `user.getRecentTracks`
  with its scrobbler key (error 6). The blocker is left out of every list
  of people the blocked account reads: follow lists, search, the
  listeners of an artist or a track, comments, image candidates, and its
  feed and community charts, which cover only followed accounts anyway.
- **Neither is told**: nothing in any response says who blocks whom, the
  block list is its owner's, and the export lists the blocks made, never
  the blocks received. The blocked account can still infer it: the
  blocker vanished from its follows like a deleted account would, and a
  public profile is anyone's to see signed out, which is where the two
  differ. A block hides an account from an account, not from a person.
- **The blocker** no longer sees the blocked account in lists of people
  (follow lists, search, listeners, comments, image candidates), but can
  still open its profile, which says `blocking: true`, to lift the block.
- **Live streams**: a block closes the streams of the blocker's now playing
  the blocked account holds open, on every API instance (a close message on
  the blocker's Redis channel, which no stream forwards).
- **What stays**: counts (`followers_count`, catalog counts, rankings)
  were made of follows that ended or of plays nobody is named in; an image
  the blocked account uploaded that became an artist's or album's image is
  the catalog's and stays.

**The block list** (`GET /v1/user/me/blocks`): `limit` (50, at most 200)
and `before`, as follow lists. Entries are `BlockEntry`: `id`, `username`,
`blocked_at`, and `display_name` and `image_url`, both `null` for a private
account (the blocker knows its name, nothing more). Accounts waiting to be
deleted are left out; their blocks stay and can be lifted by username.

```json
{
  "entries": [
    { "id": 812, "username": "spammer", "display_name": null,
      "image_url": null, "blocked_at": "2026-10-13T18:02:11Z" }
  ],
  "next": null
}
```

**Removing a follower** (`follower_removals`) ends its follow without
blocking it: profiles stay visible both ways, and it can't follow again for
30 days (a 409 saying it was removed); the worker deletes removals past
their date. Removing an account that doesn't follow records nothing.

**Limits** (`limits::block_attempt`, `limits::pair_attempt`, Redis, fail
open: a block protects its maker):

- 10,000 blocks per account (`MAX_BLOCKS`, 409 past it).
- 200 blocks, unblocks and removed followers an hour per account, 600 per
  address, refused ones included.
- 3 blocks of one account by another a day, and 3 new follows of one
  account by another a day (following again after an unfollow counts;
  following what is followed already doesn't): following, unfollowing and
  following again put the follower first in the other's list each time,
  and blocking and unblocking made a profile come and go. Without
  notifications that is all such a cycle can do, now three times a day.
- Follows, blocks and removals take the pair's advisory lock: no follow
  lands beside a block made at the same moment.

**Deleting and restoring**: an account waiting to be deleted keeps its
blocks and removals, and a restore finds them in place; the erase removes
every block and removal it made or received (`ON DELETE CASCADE`, the
generic erasure test covers both tables both ways).

## Charts of a user

`GET /v1/user/{username}/charts/{artists,tracks,albums}`: public, with the
profile's privacy (403 for someone else's private profile, 404 for a
missing or leaving one). Entries are the profile's top lists' `TopArtist`,
`TopTrack` and `TopAlbum`, in a page (`ArtistChart`, `TrackChart`,
`AlbumChart`) that says what it covers.

| parameter | | |
| --- | --- | --- |
| `period` | `7d` (default), `30d`, `90d`, `12m` (365 days), `all` | UTC days ending today, today included; unknown: 400 |
| `from`, `to` | `YYYY-MM-DD`, both included | instead of a period: both or neither, `from` ≤ `to` ≤ today, 366 days at most; else 400 |
| `limit` | 50, clamped to 1–200 | |
| `offset` | 0 | negative: 400; past the end: an empty page with its `total` |

```json
{
  "period": "30d", "from": "2026-09-14", "to": "2026-10-13",
  "total": 212, "offset": 0, "limit": 2,
  "entries": [
    { "album_id": 31, "album_title": "Mezzanine", "artist_id": 97,
      "artist_name": "Massive Attack", "image_url": "https://…/albums/3f/….jpg",
      "play_count": 41, "unverified_count": 3 },
    { "album_id": 8, "album_title": "Dummy", "artist_id": 12,
      "artist_name": "Portishead", "image_url": null,
      "play_count": 30, "unverified_count": 0 }
  ]
}
```

`period` is `null` for a custom range and `from` for `all`.

- **Plays are the profile's:** suspect ones included (and counted in
  `unverified_count`, with plays of tracks without a known length), the
  classifier's duplicates left out; today's count as they arrive; a play
  counts until it is classified.
- **Albums:** each track belongs to one album, the first a scrobble of it
  named (`tracks.album_id`), and its plays count for that album, from
  whichever release they were played, in charts and in the album's own
  count (`GET /v1/album/{id}`, `null` below `RANKING_MIN_LISTENERS`
  listeners like a track's or an artist's, migration `0029`). A track
  filed under no album isn't in album charts.
- **Cost:** the daily aggregates and the classifier's flags, never the
  scrobbles hypertable (`db::queries::charts`). A page is cut before names
  and images are looked up.
- **Caching:** a weak `ETag` made of the profile's counts
  (`scrobble_breakdown`) and the hour, 304 on `If-None-Match`;
  `Cache-Control: private, max-age=60` and `Vary: Authorization`, since
  any profile can turn private. The server keeps each computed page in
  Redis under that version for 15 minutes, for every viewer allowed to see
  it, so a late play reaching an old day at the aggregates' refresh, or a
  new name or image, can take up to an hour to show.
- The profile's older `top-artists` and `top-tracks` routes read the same
  queries with their own periods and limit (50).

## Community charts

`GET /v1/user/me/community/{artists,tracks,albums}` (`read` scope): what
the accounts the signed-in user follows played, ranked as the global
rankings are ([rankings.md](rankings.md#listeners-and-plays)): by
listener credits, then weighted plays, the coordination factors applied,
both rounded. Entries are `RankedArtist`, `RankedTrack` and `RankedAlbum`.
Same parameters as a user's charts, except **no `all`** and ranges within
the kept weights (from `retained_from(today)`, a year and a week back):
older weights are purged.

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

- **Accounts:** the visible accounts followed (`following`), the most
  recently active first (`users.last_seen_at`), as many as the range
  allows (`accounts`): a chart reads 14,600 (account, day) pairs of
  `ranking_daily` at most (`shared::charts::COMMUNITY_USER_DAYS`), so its
  cost doesn't grow with the follows. Every follow (2,000) over a week, 486
  over 30 days, 162 over 90, 40 over a year.
- **Small groups:** entities fewer than `RANKING_MIN_LISTENERS` (5) of
  those accounts played aren't served, the global rankings' rule; a credit
  is one account at most, so following fewer than 5 visible accounts gives
  an empty chart. Without it, a chart of one or two follows would show one
  account's weights: whether its plays were discounted, and by how much
  (a flagged account's coordination factor). Raw counts are never served;
  nothing names an account.
- **Cache:** a computed ranking (entity, credits, weight; 1,000 positions
  at most) is kept 5 minutes in Redis, keyed by the kind, the days, the
  minimum and a hash of the sorted account ids. Pages and repeat views read
  it; a follow, an unfollow, or a followed account turning private or
  leaving changes the accounts, hence the key, so nothing needs
  invalidating and no viewer gets a ranking of accounts they don't cover.
  Viewers covering the same accounts share one. A weak `ETag` (the key's
  hash and when it was computed) gets a 304; `Cache-Control: private,
  max-age=60`. Redis failing costs only the computation.
- **Plans:** the chart's transaction sets `plan_cache_mode =
  force_custom_plan` and `enable_mergejoin = off`: after five runs Postgres
  moved the prepared statement to a generic plan three times as slow.

## Feed

`GET /v1/user/me/feed` (`read` scope): the last 14 days (`FEED_DAYS`) of
scrobbles of the visible followed accounts, newest first. Suspect plays and
duplicates are left out; plays not classified yet are in with `status:
null`.

```json
{
  "entries": [
    { "id": 912873, "played_at": "2026-10-13T10:41:07Z",
      "user": { "id": 70, "username": "ana", "display_name": "Ana", "image_url": null },
      "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_id": 31, "album_title": "Mezzanine",
      "album_image": "https://…/albums/3f/….jpg", "artist_image": null,
      "status": "counted" }
  ],
  "next": "1760352067000000.912873"
}
```

- **Entries:** the play's album, else the track's catalog album; the
  artist's image for when there is no cover; `status` `counted`,
  `no_data` or `null`. The scrobble's `id` makes entries unique.
- **Pages:** `limit` (50, at most 100) and `before` (the previous page's
  `next`, an opaque keyset cursor of (`played_at`, `id`); invalid: 400).
  A page stays where it was however many plays arrive: plays newer than a
  cursor never show on the pages after it (one at the cursor's very time
  sorts above it by its newer id), late ones below it do. `next` is `null`
  once a page reaches the 14th day; a page can hold fewer entries than
  asked for and still have a next one. Clients poll the first page and
  stop paging back at an entry they already have. No stream.
- **Cost:** a page walks back from its cursor in windows of an hour, then
  four times longer each, until it is full or 14 days back. Each window
  reads by account and time (`idx_scrobbles_user_time`) the accounts whose
  latest play is in or after it, and only within chunks compression hasn't
  reached (it starts at 30 days). That needs `users.last_seen_at` to be the
  latest play: the counter trigger let a late play move it back (a phone's
  offline queue); it now keeps the greater, as imports did, and migration
  `0030` restored it (0.95 s for 4,006 accounts).
- `Cache-Control: private, no-store`.

## Access

| route | access |
| --- | --- |
| `GET /v1/user/{username}/charts/{artists,tracks,albums}` | public (a session sees its own private profile) |
| `GET /v1/user/{username}/followers`, `/following` | public (likewise) |
| `GET /v1/album/{id}` | public |
| `GET /v1/user/me/community/{artists,tracks,albums}` | `read` |
| `GET /v1/user/me/feed` | `read` |
| `POST`, `DELETE /v1/user/{username}/follow` | `write` |
| `POST`, `DELETE /v1/user/{username}/block` | session |
| `GET /v1/user/me/blocks` | session |
| `DELETE /v1/user/me/followers/{username}` | session |

Public routes take an optional session (`optional_auth`); an API token,
even with `read`, is ignored there, as on every profile route. Search, the
listeners of an artist or a track and comments take one too, to leave
blocked and blocking accounts out. Blocks take a session: a leaked token
mustn't show whom the user protects themselves from, nor lift a block (the
apps' interfaces use the session anyway). `router/tests.rs` lists each.

## Abuse

- **Seeing a private account by following it:** closed (above).
- **Follow farms:** a farm following someone fills their follower list and
  count; the cap (2,000 followed per account) and the rates (100 an hour
  per account, 300 per address) slow it, and following opens nothing a
  private profile hides. It can't reach anyone's feed or community chart:
  those show only accounts the viewer chose to follow.
- **Flooding a feed:** an account the viewer follows can scrobble up to
  the daily quota (3,000); the classifier's suspect plays (over what real
  time allows) and duplicates are left out of the feed.
- **Probing the weights:** a community chart shows weighted numbers, which
  could tell a farm how much of its push is discounted. Entries need
  `RANKING_MIN_LISTENERS` credited accounts, as in the global rankings, so
  a farm needs as many aged accounts listening to one entity to read a
  number at all, and then reads their sum, what the global rankings already
  show it.
- **Cost:** a community chart is bounded by its user-days (about 300 ms
  here) whatever the follows, a feed page by the plays in its windows.
  Each custom range is a new cache key, so a client can still have one
  computed per request, as often as the per-address limit allows (60 a
  minute by default).

- **Harassment**: block the account (it no longer finds you signed in,
  and can't follow you), or remove it from your followers (30 days without
  following you again). A follow farm can be removed or blocked at 200
  accounts an hour.

Open: no approval for follows (private accounts can't share with chosen
people); no muting (hiding an account without ending its follow); a
blocked person can make a new account, or read a public profile signed
out; a private account following the user isn't in the user's own
follower list (lists never name private accounts), so it can be removed or
blocked only by its username; the rate limits fail open without Redis.

## Measured

On the synthetic population ([rankings.md](rankings.md#synthetic-scenario):
4,000 accounts over 60 days, with `follows.sql` and `heavy.sql`: 4,100,001
scrobbles, 314,903 follows, 3,415,981 `ranking_daily` rows, 1,680
coordination days), chunks older than 30 days compressed, TimescaleDB
2.29.2 / Postgres 18 in a container (`shared_buffers=512MB`), the release
API and Redis on the same 8-core desktop with 7 GB of memory, one client
at a time. "Cold" means the server's caches emptied before each request
(curl, a new connection each, 15 to 20 requests: p50 and maximum); "cached"
200 requests on one connection (oha: p50 and p99).

A user's charts, cold:

| | 7d | `all` |
| --- | --- | --- |
| typical account (60 days, ~1,000 plays): artists, tracks, albums | 5.0, 7.4, 5.8 ms (max 16) | 30, 14, 36 ms (max 44) |
| 146,691 plays over three years, artists | | 122 ms (max 184) |
| the same, tracks | | 146 ms (max 155) |
| the same, albums | | 310 ms (max 336) |

The same account over 12 months: 52, 62 and 127 ms. Cached: 1.5–1.8 ms
p50, 3.7 ms p99 at most, whatever the history. Before the page was cut
before its names were looked up, the three-year all-time track chart took
463 ms (p50).

Community charts of tracks, cold (p50 / max), by the number of accounts
followed:

| follows | 7d | 30d | 90d |
| --- | --- | --- | --- |
| 10 | 11.5 / 54 ms | 14.6 / 18 ms | 17.8 / 25 ms |
| 35 (the median) | 15.0 / 55 ms | 27.0 / 46 ms | 40.4 / 61 ms |
| 100 | 23.2 / 42 ms | 58.7 / 95 ms | 94.4 / 138 ms |
| 121 (p90) | 25.4 / 59 ms | 64.8 / 104 ms | 103.8 / 159 ms |
| 500 | 81.4 / 157 ms | 249.5 / 404 ms | 168.5 / 245 ms |
| 1,000 | 150.0 / 245 ms | 270.0 / 390 ms | 166.3 / 220 ms |
| 2,000 | 276.8 / 396 ms | 277.9 / 369 ms | 168.5 / 207 ms |

Artists take about two thirds of that (2,000 follows, 30 days: 187 ms),
albums about 1.4 times (401 ms: every (account, track) is mapped to its
album). The history is 60 days long, so 90 days reads 60 and 12 months
(40 accounts) only 2,400 user-days, 51 ms; a real year reads 14,600 like
30 days with 486 accounts, about 270 ms by the cost model below
(unverified). Cached: 2.6 ms p50 for 35 follows, 9.3 ms for 2,000 (the
followed accounts are read on every request), deep pages the same.

The cost model, from the query alone in psql: linear in (account, day)
pairs, about 18–20 ms a thousand (700: 25 ms; 3,000: 60; 7,000: 176;
14,000: 268; 30,000: 460–520; 60,000: 880; 120,000: 1.5 s). The budget of
14,600 holds every chart near 300 ms on this machine. A covering index on
`ranking_daily (user_id, day)` saved only 10–15 % for 192 MB more, so
there is none. Before the custom plan, 2,000 follows over a week took
858 ms.

The feed (p50 / p99), a first page, a page 3 days back and one 10 days
back (the newest plays were 1.5 days old, so a first page walked back
through four windows):

| follows | first page | 3 days back | 10 days back |
| --- | --- | --- | --- |
| 10 | 8.6 / 12 ms | 21.4 / 26 ms | 16.7 / 20 ms |
| 35 | 8.6 / 11 ms | 14.5 / 18 ms | 8.7 / 11 ms |
| 121 | 8.7 / 24 ms | 8.3 / 11 ms | 9.1 / 13 ms |
| 500 | 10.5 / 23 ms | 10.2 / 13 ms | 11.1 / 14 ms |
| 1,000 | 11.4 / 14 ms | 11.1 / 14 ms | 14.1 / 18 ms |
| 2,000 | 15.5 / 18 ms | 15.1 / 19 ms | 20.5 / 24 ms |

Follows (p50 / p99): a first page of the most followed account's 3,721
followers 2.2 / 5.0 ms, 200 of them 4.3 / 7.2 ms; a profile with its
counts 3.3 / 6.6 ms; `/friends` 3.5 / 5.7 ms.

```bash
psql "$SCRATCH_URL" -v users=4000 -v days=60 -f scripts/synthetic/population.sql
psql "$SCRATCH_URL" -f scripts/synthetic/follows.sql
psql "$SCRATCH_URL" -f scripts/synthetic/heavy.sql
DATABASE_URL="$SCRATCH_URL" worker classify reclassify   # by date range, in parallel
DATABASE_URL="$SCRATCH_URL" worker rank recompute
DATABASE_URL="$SCRATCH_URL" worker rank refresh
```

## Limits

- **A year's community chart covers 40 accounts:** the most recently
  active of those followed. Following more is no help over long ranges;
  the response says how many it covered (`accounts`) of how many it could
  (`following`).
- **Albums by the catalog's track:** a track on several releases counts
  for the first one a scrobble named, so a deluxe edition's chart holds
  only its bonus tracks, and whoever scrobbles a track first chooses its
  album for everyone. Community album charts also map every (account,
  track) to its album by joining `tracks`, which grows with the catalog.
- **First views of long histories:** a cold all-time album chart of an
  account with 150,000 plays takes ~300 ms, more with more; only repeat
  views are cached.
- **Feed reach:** 14 days, and a feed doesn't merge or collapse repeats
  of one account.
- **No follow approval, no muting, no notifications.**
