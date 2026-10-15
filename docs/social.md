# Social: follows, charts and the feed

What users see of each other: follows, a user's own charts, the charts of
the accounts a user follows (community charts), a feed of what those
accounts play, loved tracks, what listening says about artists and
people (similar artists, taste neighbours, recommendations), and what
the community writes about the catalog (bios and tags). Global rankings are in
[rankings.md](rankings.md); what is stored and erased about an account, in
[privacy.md](privacy.md).

## Who sees whom

One rule, applied everywhere an account's data or name is shown: an
account is **visible** to a viewer when it isn't waiting to be deleted
and it is public, or it is the viewer's own.

- **A private profile is its owner's alone.** Its profile, follow lists,
  recent scrobbles, top lists, charts, heatmap, loved tracks and live stream answer 403 to
  everyone else, followers included (`middleware/visibility.rs`). Following
  takes no approval, so a follower is anyone who clicked follow; until
  2026-10-12 followers could see a private profile, which let anyone see
  any. The Last.fm-compatible API already showed a private profile to its
  owner only.
- **Lists of people** (followers, following, listeners of an artist or a
  track, search) leave out private accounts, except the viewer's own entry,
  and accounts waiting to be deleted (404 on their own routes). Taste
  neighbours name only accounts that chose to be shown, while public
  ([below](#taste-neighbours)); similar artists and recommendations name
  nobody.
- **Community charts and the feed** cover only visible followed accounts:
  following a private account adds nothing to either.
- **Counts** (`followers_count`, `following_count`, catalog counts, global
  rankings) include private accounts, unnamed, as every aggregate does.
- Comments, uploaded images and bio revisions keep their author's name
  whatever the privacy setting: they were posted for others to see. A
  user's tags are the profile's, as visible as it; tag counts name
  nobody.
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
- **Albums:** each track belongs to one album (`tracks.album_id`, decided
  by the accounts playing it: [Albums](#albums)), and its plays count for
  that album, from whichever release they were played, in charts and in
  the album's own count (`GET /v1/album/{id}`, `null` below
  `RANKING_MIN_LISTENERS` listeners like a track's or an artist's,
  migration `0029`). A track filed under no album isn't in album charts.
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

## Albums

Each track belongs to one album (`tracks.album_id`), and its plays count
for that album, from whichever release they were played, in a user's
charts, in community charts and in the album's own count. Until migration
`0032` that album was the first one a scrobble named, so one account could
file a popular track under any album for everyone.

- **Every account has one say per track** (`track_album_votes`): the album
  its plays of the track named, the latest when they change; a play
  arriving late (a phone's queue) doesn't take it back. Every live source
  records it (`ingest_scrobble`: the native API, the scrobbler-compatible
  ones, Spotify) and so do imports; `worker albums backfill` makes the
  history's. Recording it costs a scrobble one primary-key probe, and a
  write only when the account's album changes.
- **An album's weight** is one per account behind it that is at least 7
  days old (`RANKING_MIN_ACCOUNT_DAYS`, as for rankings) and not waiting to
  be deleted; an account the coordination detection flagged counts at its
  lowest factor of the last 28 days. MusicBrainz listing the track's
  recording on a release of that title (`tracks.mb_release_titles`,
  lowercased, from the recording's last lookup) doubles it.
- **The heaviest album takes the track only with half again the current
  album's weight and one account more** (`shared::albums`): 3 accounts take
  it from 2, 15 from 10, and going back from 15 takes 23, so a close race
  doesn't flip it back and forth. A tie stays; an album with nobody left
  behind it gives way to any.
- **The worker decides** (the `album attribution` loop, every 30 s, 200
  tracks a transaction) a track 30 seconds after an account's say on it
  changed (a burst costs one decision), when an account behind one of its
  albums comes of age, when one asks for deletion, is restored or erased,
  and when MusicBrainz's releases for it arrive or go. A coordination run
  re-decides nothing by itself: a flagged farm's albums go back at the
  track's next vote, which popular tracks get all the time, or at the next
  backfill.
- **A new track** takes the first album named, as before, until the worker
  decides otherwise: being first decides nothing once others disagree.
- **Reads don't change**: every chart and count reads `tracks.album_id`
  when computed. A move shows in a cached personal chart within the hour
  (its version is the hour), in a community chart within 5 minutes.
- **MusicBrainz release titles** come with every recording lookup the
  enrichment makes; tracks enriched before `0032` get theirs from the
  re-sweep (200 a sweep, most played first, at MusicBrainz's 1 request a
  second) or a forced refresh (`POST /v1/track/{id}/refresh`); `worker
  tracks mb-review --apply` clears them with the match.

**`worker albums backfill [--dry-run] [--limit N]`**, once after migrating
to `0032`: every stored scrobble's say, read a chunk a statement (no long
transaction holds a chunk) into a table of its own connection, then every
track with votes decided from them; it reports how many tracks move and
the most listened to of them, and without `--dry-run` merges the votes (a
later live one stays) and moves the tracks in one transaction.

**What an attack takes** (`scripts/synthetic/album-attack.sql`, on the
synthetic population with albums, below; every farm account established
and unflagged, the worst case):

| attack | the track's album | farm | album after |
| --- | --- | --- | --- |
| 2,000 accounts, one play each, on the most listened-to track | 3,006 accounts | 2,000 | unchanged |
| one account playing it 1,000 times in a day | 3,006 | 1 | unchanged |
| a track 98 accounts file under its album | 98 | 146 | unchanged |
| a track 99 accounts file under its album | 99 | 149 | the farm's |

Moving a track takes accounts, not plays: half again as many as stand
behind its album, plus one, each at least a week old, each having played
the track from the attacker's album last, none flagged by the coordination
detection (a flagged one counts at its factor), and three times as many
when MusicBrainz lists the recording on the real album and not the fake
one. A track three people listen to takes five such accounts; a popular one
takes thousands, which a farm that size has to keep aged, unflagged and
playing, and lose again as soon as the real listeners outnumber it by half.
An attacker who also controls MusicBrainz data (a release of their title
listing the recording) halves that. Being first on a new track only holds
it until two accounts disagree.

**Measured** on the synthetic population (4,006 accounts, 4,100,001
scrobbles, 152 of 157 chunks compressed; `albums.sql`: 85 % of accounts
name albums, one in ten of those the deluxe edition, each track first
filed under the album its earliest scrobble named):

- `backfill --dry-run`: 895,835 votes, 107,120 tracks, 95 s; 4,395 tracks
  would move, every one from a deluxe edition scrobbled first to the
  standard edition most accounts named (Track 2 of Synth Artist 2: 295
  accounts on the deluxe, 2,639 on the standard). With the attacks: 85 s
  dry, 197 s applied.
- Votes: 898,131 rows, 87 MB (52 MB of table), a fifth of what
  `ranking_daily` takes on the same population (459 MB): one row per
  account and track ever played naming an album.
- Deciding 200 ordinary tracks takes 7 ms; the 200 most voted (228,468
  votes) 630 ms, during which a scrobble of one of them waits that long to
  record its say.
- Recording a say that didn't change: 111 µs a scrobble (server side), no
  write.
- Community album charts join `tracks` once per (account, day, track)
  row: 1.1 to 1.3 times the tracks chart (2,000 follows over 7 days 366 ms,
  536 before; 100 over 7 days 38 ms, 88 before). An album column in
  `ranking_daily` reached 243 ms, for 8 bytes a row (+13 % of its table)
  and an index on `track_id` to rewrite a track's rows whenever its album
  changes: not kept.

```bash
psql "$SCRATCH_URL" -f scripts/synthetic/albums.sql       # after follows.sql, before 0032
DATABASE_URL="$SCRATCH_URL" worker migrate
DATABASE_URL="$SCRATCH_URL" worker albums backfill --dry-run
psql "$SCRATCH_URL" -f scripts/synthetic/album-attack.sql
DATABASE_URL="$SCRATCH_URL" worker albums backfill
psql "$SCRATCH_URL" -f scripts/synthetic/album-attack-results.sql
```

Open: a new coordination run doesn't re-decide the tracks a flagged farm
voted on (their next vote does); a track whose listeners split between two
editions stays with whichever came first until one has half again the
other's accounts; an account's say follows its latest change of album, not
its latest play (a late play naming the album it left doesn't move it back);
the personal album chart still sums by track and then by album (310 ms
cold for three years of plays, unmeasured since).

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

## Loved tracks

A user can love a track, as on Last.fm: a list on the profile, a heart on
the track's page, in recent scrobbles and in the feed. Loves are what a
user says about a track, not listening, so they are shown and kept but
**feed nothing**: the rankings, their weights and coordination factors,
similar artists, recommendations, taste neighbours, album attribution and
the classifier don't read them, and no "loved by N" number is published
for a track. A count of loves would be the cheapest number on the server
to inflate (a love takes no listening, nothing to classify or weigh);
behind `RANKING_MIN_LISTENERS` it would be `null` for almost every track
of a small server anyway, and five accounts could still put any number on
any track. Giving loves a say anywhere would first need anti-botting of
their own (account age, coordination), and is not done.

| route | access | |
| --- | --- | --- |
| `POST /v1/track/{id}/love` | `write` | 204, also when loved already (the first `loved_at` stands); 404 not in the catalog, 409 at the cap, 429 past the rate |
| `DELETE /v1/track/{id}/love` | `write` | 204, also when not loved; 404 not in the catalog, 429 past the rate |
| `GET /v1/user/{username}/loved` | public (a session sees its own private profile) | a page, newest love first |
| `GET /v1/user/me/loved` | `read` | the same, whatever the profile's privacy |

```json
{
  "entries": [
    { "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_id": 31, "album_title": "Mezzanine",
      "album_image": "https://…/albums/3f/….jpg", "artist_image": null,
      "loved_at": "2026-10-13T10:41:07.123456Z" }
  ],
  "total": 132,
  "next": "1760352067123456.1841"
}
```

- **Pages** (`LovedTrackPage`): `limit` (50, at most 200) and `before` (the
  previous page's `next`, a keyset cursor of (`loved_at`, `track_id`);
  invalid: 400); `next` is `null` on the last page. `total` counts every
  track the user loves: what paging to the end would show anyway. Each
  entry is the track with its artist and the album the catalog files it
  under (`album_image`, else `artist_image`, for artwork; both nullable,
  and `album_id`/`album_title` without an album).
- **Flags:** `GET /v1/track/{id}` takes an optional session and says
  whether the viewer loves the track (`loved`: `true`/`false`, `null`
  signed out; an API token, even with `read`, counts as signed out there,
  as on every optional-auth route). Recent scrobbles (`ScrobbleRich`) and
  feed entries (`FeedEntry`) say whether the account that played the track
  loves it now (`loved`), whoever looks: it is that profile's, as visible
  as the rest of it. Now playing (`NowPlayingRich`, also on the live
  stream) carries `track_id`, so an app can love what is playing.
- **Visibility:** the profile's ([above](#who-sees-whom)): a private
  account's loves are its owner's alone (403), an account waiting to be
  deleted or blocking the signed-in viewer has none to show (404, as for a
  missing one), and the Last.fm-compatible API answers the same (17, 6).
- **Limits:** 50,000 loved tracks per account (`MAX_LOVED_TRACKS`, 409),
  counted under a per-account lock so concurrent loves can't pass it; 300
  loves and unloves an hour per account and 1,000 per address
  (`limits::love_attempt`, every API together, refused attempts included,
  Redis, fails open like follows).
- **The catalog:** the native routes love tracks by id and create
  nothing. The Last.fm-compatible API loves by artist and title, resolved
  as scrobbling resolves them (same normalization, credits, enrichment);
  a track the server hasn't seen is created, as now playing would create
  it, bounded by the love limits. An unlove by name only looks the track
  up. Imported loves create tracks as imported scrobbles do.
- **When the catalog changes:** a love points at the track, so it shows
  whatever album the track's listeners have filed it under
  ([Albums](#albums)); a track removed from the catalog takes its loves
  with it (`ON DELETE CASCADE`, indexed by track). Tracks are never merged
  today; a merge would have to move loves to the surviving track, keeping
  the earlier `loved_at` where an account loved both.
- **Deletion and export:** loves stay through the deletion grace period
  (hidden with the profile) and come back with a restored account; the
  erase deletes them with the user row; the export holds them
  (`loved_tracks.csv`, `loved_tracks.json`, [privacy.md](privacy.md)).

### From other apps

Each love records its source (`scrobblr`, `lastfm`, `audioscrobbler`,
`listenbrainz`, `lastfm_import`) and, where the protocol names one, the
client. A scrobbler token may love, as a Last.fm session may: it is what
those apps do with it, and a leaked one can add or take back loves,
nothing more. Every love and unlove counts against the same hourly limits
and the cap as the native routes. [scrobbler-clients.md](scrobbler-clients.md)
says what each app does.

**Last.fm-compatible API** (`/2.0/`):

- `track.love` and `track.unlove` (POST, signed and authenticated like
  `track.scrobble`: a session key or a scrobbler token, 120 requests a
  minute). Answered `<lfm status="ok"></lfm>` or `{}`; errors 6 for a
  missing artist or track and at the cap, 9, 13, 29. A love of a track the
  catalog doesn't have creates it, as scrobbling would; an unlove never
  does.
- `user.getLovedTracks` (numbered pages, 50 by default, 1,000 at most) and
  `user.getRecentTracks` with `extended=1` (the artist as an object, and
  `loved`), as Pano Scrobbler asks for them.
- `track.getInfo`, by `artist` and `track` (normalized as scrobbling does:
  case and surrounding spaces aside) or by `mbid`, never creating a track
  (error 6 when the catalog doesn't have it). With `username`, or with a
  session and no `username`, it adds `userloved` and `userplaycount` (the
  profile's count, duplicates left out) where that profile may be seen:
  anyone's public one, the session's own private one. They are left out,
  without saying why, for a private account, one waiting to be deleted,
  one blocking the session's account, or a name that doesn't exist. There
  is no `listeners` (never kept), and `playcount` only for a track
  `RANKING_MIN_LISTENERS` accounts played, as on the track page.
  `autocorrect` is accepted and ignored. pylast reads it; Pano Scrobbler
  and Web Scrobbler ask last.fm itself for track info.

```xml
<lfm status="ok"><track><name>Roads</name><mbid>…</mbid><url></url>
<duration>305000</duration><streamable fulltrack="0">0</streamable>
<artist><name>Portishead</name><mbid></mbid><url></url></artist>
<album><artist>Portishead</artist><title>Dummy</title><mbid></mbid><url></url>
<image size="small">https://…</image>…</album>
<userplaycount>12</userplaycount><userloved>1</userloved><toptags></toptags></track></lfm>
```

**Audioscrobbler 1.2**: a submitted play rated `L` loves its track.

**ListenBrainz-compatible API** (`/1/feedback/…`, the scrobbler token as
`Authorization: Token …`):

- `POST /1/feedback/recording-feedback` with `recording_mbid` or
  `recording_msid`, and `score`: 1 loves the recording's track, 0 takes
  the love back. -1 (hate) takes the love back too, since a recording has
  one score on ListenBrainz, and is not stored: nothing about hates is
  kept or shown. 400 for any other score, no recording, an invalid UUID
  or another key; 404 for a love of a recording the catalog doesn't have
  (scrobble it first; nothing is created); an unlove or hate of one is
  `ok`; 409 at the cap; 429 past the limits.
- **Recordings**: an MBID names the track MusicBrainz enrichment matched
  to it, else the most played track a client sent it for as a hint that
  enrichment hasn't decided yet nor rejected it for (indexed, migration
  `0036`). An MSID is one this server handed out: a version-8 UUID holding
  the track's id (`5c40bb1e-7e57-8001-8000-…`), stored nowhere, so an MSID
  from listenbrainz.org never names anything here. `submit-listens` with
  `return_msid` answers a `playing_now` with the track's
  (`{"status": "ok", "recording_msid": "…"}`), as ListenBrainz does; Pano
  Scrobbler keeps it on its now-playing notification, whose love button
  sends it back.
- `GET /1/feedback/user/{user_name}/get-feedback` (`count` 25 by default,
  1,000 at most, `offset`, `score`, `metadata=true`): the user's loves,
  newest first. `score=-1` lists nothing; 0 or anything else is a 400.
- `GET` or `POST /1/feedback/user/{user_name}/get-feedback-for-recordings`
  (`recording_msids`, `recording_mbids`, 1,000 at most): the score of each,
  1 if loved, else 0, unknown recordings included.
- Both reads are as visible as the profile: 404 for an account that
  doesn't exist, waits to be deleted or blocks the token's account, 403
  for a private one but to its owner's token. Like ListenBrainz they take
  no token; an invalid one is ignored, a valid one counts against its
  account's request limit.
- ListenBrainz's per-recording lists (who loved a recording, and how many:
  `/1/feedback/recording/…`) aren't served: loves have no public count.

```json
{
  "feedback": [
    { "created": 1760352067, "recording_mbid": null,
      "recording_msid": "5c40bb1e-7e57-8001-8000-000000000729", "score": 1,
      "track_metadata": { "artist_name": "Portishead", "track_name": "Roads",
        "release_name": "Dummy",
        "additional_info": { "duration_ms": 305000,
          "recording_msid": "5c40bb1e-7e57-8001-8000-000000000729",
          "recording_mbid": null } },
      "user_id": "ana" }
  ],
  "count": 1, "total_count": 132, "offset": 0
}
```

A track with an MBID also carries `recording_mbid` and `mbid_mapping`
(`recording_mbid`, `release_mbid`, `artist_mbids`).

### Imported from Last.fm

A Last.fm import brings the account's loved tracks first
(`user.getLovedTracks`, 1,000 a page), each page committed with its cursor
under the import's lease, so it resumes after a slice, an error or a dead
worker without loving anything twice; Last.fm's rate limit and errors are
handled as for the history. A track already loved here keeps its love
and time; loves stop at the cap, newest kept (`loved_error:
cap_reached`), and don't count against `LASTFM_IMPORT_MAX_SCROBBLES`. A
re-import reads only loves newer than its window, so a love taken back
here isn't brought back; `full` rereads them all. If Last.fm won't show
them (a hidden profile not signed with the user's session, or another
lasting error), they are skipped (`loved_error`) and the history goes on.
Progress: `loved_total` (what Last.fm lists), `loved_imported` (added),
`loved_done`, `loved_error` on the import.

### Measured

On the synthetic population ([Measured](#measured-2) below describes the
setup: 3,953,310 scrobbles, 314,903 follows) with `loves.sql`: 310,414
loves over 4,001 accounts (median 38 an account, p90 171) and one
account, `lover`, at 49,999, the worst case. 51 MB in all (20 MB heap,
31 MB of indexes), about 170 bytes a love. The release API, Redis and
TimescaleDB 2.29.2 / Postgres 18 in a container on the same desktop, one
client at a time; latencies over HTTP, p50 / p99.

| | p50 | p99 |
| --- | --- | --- |
| love a new track (account with 35 loves; 200 curls) | 9.3 ms | 19.7 ms |
| love it again / unlove it | 5.5 / 5.3 ms | 18.8 / 15.9 ms |
| love the 50,000th track / unlove at 50,000 | 17.4 / 4.9 ms | 29.4 / 8.0 ms |
| love past the cap (409) | 14.0 ms | 44.3 ms |
| loved tracks, 35 loves (oha, 200 requests) | 3.8 ms | 11.2 ms |
| 49,999 loves: first page of 50 / of 200 | 15.2 / 19.5 ms | 39.5 / 51.6 ms |
| 49,999 loves: a page of 200, 40,000 deep | 27.9 ms | 38.2 ms |
| `user.getLovedTracks`, 1,000 a page: first / 49th | 78.5 / 95.4 ms | 112 / 169 ms |
| `GET /v1/track/{id}` signed in (with `loved`) | 6.4 ms | 14.3 ms |

Counting a list of 50,000 loves (the `total`, and the cap's check before
a love) takes 10–13 ms of those times, an index-only scan; a typical
account's is negligible. A counter kept by trigger would make both
constant, at a write per love; not worth it at these numbers. A
1,000-entry Last.fm page is built in both XML and JSON, as every answer
of that API is.

The `loved` flags, in psql (medians of 30 interleaved runs, the account
with the most loves of the population, 390): 200 recent scrobbles 5.2 ms
without, 7.3 ms with (about 10 µs a row, a primary-key probe); a feed
page of 100 for an account following 121, 0.38 ms without, 0.60 ms with.

Scrobbling is untouched: no trigger or function on `scrobbles` reads
`loved_tracks`, and ingest doesn't touch it. 200 native scrobbles, with
the 310,414 loves and with the table emptied, twice each in turn: p50
25.6 and 28.8 ms with, 21.4 and 29.1 ms without, within this desktop's
run-to-run noise. Deleting 50,000 loves, what they add to an erase,
takes 63–83 ms.

The scrobbler APIs' reads and loves, on the same data loaded again
(310,859 loves), against the account with 38 loves and the one at 49,999,
over HTTP, p50 / p99:

| | p50 | p99 |
| --- | --- | --- |
| `track.getInfo` by name, no user (oha, 200 requests) | 1.4 ms | 6.0 ms |
| the same with `username` / with the account's own session | 3.1 / 3.1 ms | 26.2 / 11.9 ms |
| by MBID: matched, with `username` / a client's hint, no user | 3.5 / 2.2 ms | 8.6 / 5.2 ms |
| a track the catalog doesn't have (error 6) | 0.5 ms | 1.5 ms |
| ListenBrainz love by MSID / unlove / a hate of nothing loved (200 curls) | 7.1 / 5.8 / 3.3 ms | 20.1 / 36.5 / 5.4 ms |
| a ListenBrainz love at 49,999 loves, then past the cap (409) | 9.2 ms | 18.5 ms |
| `get-feedback`, 38 loves, plain / with metadata | 1.6 / 2.3 ms | 4.8 / 4.2 ms |
| `get-feedback`, 49,999 loves, 1,000 with metadata / 25 at offset 49,000 | 22.3 / 6.5 ms | 34.0 / 9.2 ms |
| `get-feedback-for-recordings`, 100 MSIDs (half loved) | 7.8 ms | 10.0 ms |

The track was the population's most played (92,751 plays), so its
`playcount` is served (the listener test is a few index probes);
`userplaycount` is the user's rows of the daily aggregate by (track, user)
and their duplicate flags by (user, day): 0.6 ms in psql, 0.08 ms for the
flags with 9,266 of the track's plays flagged as duplicates. One unlove
took 268 ms (the rest under 37 ms), unexplained. A token's requests count
against its account's 120 a minute: an oha run past that got 429s, as it
should.

```bash
psql "$SCRATCH_URL" -v users=4000 -v days=60 -f scripts/synthetic/population.sql
psql "$SCRATCH_URL" -f scripts/synthetic/follows.sql
psql "$SCRATCH_URL" -f scripts/synthetic/loves.sql
```

## Bios

Artists and albums have a bio, written by the community as a wiki is:
every edit is a revision, kept for good and shown with its author, time
and summary, and anyone allowed to edit can restore an earlier one.
Tracks have none: few have anything to say that their album's bio
doesn't, and every bio is one more page to watch for spam.

| route | access | |
| --- | --- | --- |
| `GET /v1/{artist,album}/{id}/bio` | public (a session hides authors as below) | the current text (`Bio`) |
| `GET /v1/{artist,album}/{id}/bio/history` | public (likewise) | published revisions, newest first (`BioHistoryPage`) |
| `GET /v1/bio/revisions/{id}` | public (likewise; a held or rejected edit is its author's) | one revision with its text (`BioRevision`) |
| `PUT /v1/{artist,album}/{id}/bio` | `write` | 200 published, 202 held; 400, 403, 404, 409, 429 |
| `POST /v1/{artist,album}/{id}/bio/revert` | `write` | 200; 400, 403, 404, 409, 429 |
| `POST /v1/reports` | `write` | 204; 400, 404, 429 |

```json
{
  "entity_type": "artist", "entity_id": 97, "revision_id": 5012,
  "body": "Massive Attack are a trip hop group from Bristol.\n\nFormed in 1988 …",
  "kind": "edit",
  "author": { "id": 812, "username": "alice", "display_name": "Alice", "image_url": null },
  "updated_at": "2026-10-15T14:02:11.408211Z", "locked": false, "revisions": 7
}
```

A history entry (`BioRevision`) carries `id`, `entity_type`, `entity_id`,
`parent_id` (the revision current when it was written; `null` for the
first), `kind` (`edit`, `revert`, `import` from Last.fm, `moderation`),
`author` (nullable, as below), `summary` (`null` when hidden), `body`
(`null` in history pages and when hidden; the revision route has it),
`length` (characters), `reverted_to`, `status` (`published`, `held`,
`rejected`), `held_reason`, `hidden`, `current` and `created_at`. Pages:
`limit` (50, at most 200) and `before`, the previous page's `next` (a
revision id); `next` is `null` on the last page.

**Editing.** `PUT` takes `body`, `summary` (optional, one line of 200
characters) and `base_revision_id`, the `revision_id` of the bio the text
was written over (`null` when it had none). If another revision came
since, nothing is written: 409, saying which revision is current, and
the client reads the bio again and merges. Writing the current text
again is a 400. A revert (`revision_id`, `base_revision_id`, `summary`)
restores the text of an earlier revision of the same bio, published and
not hidden, as a new revision; reverts are never held. Both answer the
new `BioRevision`.

**Who may edit:** an account 7 days old (as for the rankings' weights:
fresh accounts are what farms are made of, and the server sets
`created_at`), not suspended by the operator, not waiting to be deleted,
and whose lowest coordination factor over the last 28 days is at least
0.5: the midpoint of the detection's scale, where it is more sure than
not that the account is part of a farm ([rankings.md](rankings.md#coordination)).
A refusal is a 403: a new account is told from when it may edit; the
others only that they can't edit now, so a farm learns nothing about the
detection. Nobody edits a bio the operator locked (403, `locked: true`).

**The text** is plain: no HTML, no Markdown. Clients show it as text, a
paragraph at each blank line, `http://` and `https://` URLs as links
marked `rel="nofollow ugc noopener"`, never embedded, previewed or
unfurled. The server normalizes line breaks (`\n`), tabs (spaces),
trailing spaces and runs of blank lines, and refuses (400) an empty text,
more than 10,000 characters (Last.fm's longest biographies run to a few
thousand), more than 5 links (`http://`, `https://` or `www.`), and
control, invisible or bidirectional formatting characters (embeddings,
overrides, isolates, marks, zero-width spaces, word joiners, byte-order
marks, line and paragraph separators); zero-width joiners and
non-joiners stay, since Persian, Indic scripts and emoji need them. The
summary follows the same rules on one line.

**Held for review.** An edit is held, unpublished, with `202` and
`held_reason`, when:

- a link host it adds appears more than twice in the text
  (`repeated_link`);
- its author added that host to 2 other bios in the last 7 days, or
  anyone to 4 other bios in the last 24 hours (`link_across_bios`): link
  spam pasted across the catalog;
- its text, 200 characters or more, is the text of another bio's
  revision, whatever the case and spacing (`duplicate_text`).

A held edit leaves the bio as it was, isn't in the history, and only its
author can read it (`GET /v1/bio/revisions/{id}`) until the operator
approves or rejects it ([operations.md](operations.md#moderation)).

**Limits:** 30 edits and reverts an hour per account and 60 per address
(Redis, failing open, as for loves), and, in the database whatever Redis
says, 50 revisions in 24 hours per account and 3 of one bio: two accounts
reverting each other stop after three rounds a day each, and the
operator can lock the bio. A 429 says which limit.

**Authors** are named whatever the privacy of their profile: an edit is
published for everyone, like a comment. An author waiting to be deleted,
or blocking the signed-in viewer or blocked by it, shows as `author:
null`; the text stays, being the catalog's. After an erase the
revision stays without its author ([privacy.md](privacy.md#deletion)).

**From Last.fm.** Enrichment's Last.fm bios are `import` revisions:
migration `0037` made the bios fetched before it each artist's first
revision, and enrichment adds one when an artist has no bio, or over an
earlier import on a forced refresh, and never once a person has edited
the bio, even if the text was later reverted to the import. `Artist.bio`
(`GET /v1/artist/{id}` and everything serving an artist) is the current
text; `Album` has no `bio`: read the bio route.

**Reports.** `POST /v1/reports` takes a `revision_id`, or a `tag` (on
the entity one of `artist_id`, `album_id`, `track_id` names, or
anywhere), a `reason` (`spam`, `vandalism`, `offensive`,
`personal_data`, `copyright`, `other`) and a `note` (one line, 500
characters). A second report of the same target by the same account
while the first is open changes nothing (204 either way). 20 reports an
hour per account, 60 per address. The reporter is never told what came
of it; nobody is told who reported. The queue is the operator's
(`worker moderation reports`).

## Tags

Users tag artists, albums and tracks, as on Last.fm. What a user tagged is
theirs, shown with their profile; what everyone sees on an entity is the
tags enough accounts put there, counted as the rankings count listeners.
Tags **feed nothing** else: rankings, recommendations, similar artists,
taste neighbours and album attribution don't read them.

| route | access | |
| --- | --- | --- |
| `GET /v1/{artist,album,track}/{id}/tags` | public, cacheable | the tags shown on it (`TopTags`) |
| `GET /v1/tag/{tag}/{artists,albums,tracks}` | public, cacheable | the entities a tag is shown on (`TaggedEntityPage`) |
| `POST /v1/{artist,album,track}/{id}/tags` | `write` | add up to 10 of mine; the user's tags on it after (`EntityTags`); 400, 403, 404, 409, 429 |
| `DELETE /v1/{artist,album,track}/{id}/tags/{tag}` | `write` | 204 (also when not there); 400, 404, 429 |
| `GET /v1/user/me/tags` | `read` | my tags, newest first (`UserTagPage`), `?artist=`, `album=` or `track=` for one entity |
| `GET /v1/user/{username}/tags` | public (a session sees its own private profile) | the tags a user uses and on how many entities (`UserTags`) |

```json
{ "entity_type": "artist", "entity_id": 97,
  "tags": [ { "name": "trip-hop", "taggers": 41 }, { "name": "electronic", "taggers": 12 } ] }
```

```json
{ "tag": "trip-hop", "total": 37,
  "entries": [ { "position": 1, "entity_type": "artist", "id": 97, "name": "Massive Attack",
                 "artist_id": null, "artist_name": null, "image_url": "https://…", "taggers": 41 } ] }
```

**Normalization.** A tag is stored and shown in one form: compatibility
normalization (NFKC: full-width letters, ligatures and composed accents
become their plain forms), lowercase, whitespace collapsed to single
spaces, trimmed. 1 to 40 characters of letters (any script, with their
combining marks), digits, spaces and `- & ' . + ! _`, at least one a
letter or digit; anything else (commas, slashes, angle brackets, emoji,
control and formatting characters) is a 400. `POST` takes 1 to 10;
duplicates count once. Paths take any spelling: `/v1/tag/Trip%20%20Hop/…`
is `trip hop`.

**Caps:** 10 of an account's tags on one artist, album or track, 500
different tags on one entity (past them a new tag is refused, an existing
one may still be added), 50,000 taggings per account (409); 120 tag
changes (adds and removes, through this API and the Last.fm-compatible
one) an hour per account and 300 per address (429, Redis, fails open).
The caps are counted under per-account and per-entity locks. A suspended
account can't tag (403); a new one can, its tags weighing nothing in the
counts for a week, as its plays in the rankings.

**Counts.** The worker keeps `tag_counts`: per entity and tag, every
account that put it there and their weight: an account counts in full
from 7 days after registering, at its lowest coordination factor of the
last 28 days if the detection flagged it, and not at all while suspended
or waiting to be deleted. Private accounts count, unnamed, as in every
catalog number. An entity shows a tag from `RANKING_MIN_LISTENERS` (5)
weighted accounts, `taggers` being that weight to the nearest whole. The
same minimum as every catalog count: it keeps one account (or four) from
putting a tag on anything, and a private account's tag from showing
alone. On a small server few tags reach it; each user still sees their own
tags on an entity (`GET /v1/user/me/tags?artist=…`).

The `tag counts` worker loop recounts an entity 30 seconds after its tags
change (a burst costs one pass), and every entity every 6 hours, since
weights change on their own (accounts coming of age, coordination runs,
suspensions ending); deletion requests, restores, erases and suspensions
queue the account's entities at once. It deletes tags that no account
uses and no report names, a day after they were made.

**Caching.** Both public reads send `Cache-Control: public,
max-age=300` and a weak `ETag` of the body; `If-None-Match` with it gets
a 304. `GET /v1/tag/{tag}/…` pages by `limit` (50, at most 200) and
`offset`, with `total`; an unknown tag is a 404, an invalid one a 400.

**Visibility.** `GET /v1/user/{username}/tags` is the profile's: 403 for
someone else's private profile, 404 for an account waiting to be deleted
or blocking the signed-in viewer. Tag counts name nobody.

**Blocklist.** The operator blocks terms (`worker moderation
block-tag`): a tag holding the term's words in a row is refused (400)
and taken off everything at once.

**Not imported from Last.fm.** Last.fm has no call listing a user's
tags: `user.getTopTags` gives the tag names, and `user.getPersonalTags`
the items under one tag and one kind (artist, album or track), 50 a page,
without dates. An import would cost three calls or more per tag (a user
with 500 tags: 1,500 calls, about 12 minutes of the 2 a second the import
gets), as a new resumable phase of the import job, and would land tens of
thousands of taggings at once, past the hourly limits that pace a farm.
Left for later; the loved tracks are imported, which take one call per
1,000.

### From other apps

The Last.fm-compatible API (`/2.0/`) serves what scrobblers call, read
from their sources ([scrobbler-clients.md](scrobbler-clients.md)):

- `artist.addTags`, `album.addTags`, `track.addTags` (`tags`,
  comma-separated, 10 at most) and `.removeTag` (`tag`): POST, signed and
  authenticated like `track.love` (a session key or a scrobbler token), the
  same normalization, blocklist, caps and hourly limits as the native
  routes (error 6 for an invalid or blocked tag or a cap, 29 for a limit, 4
  for a suspended account). They create nothing: an artist, album or track
  the catalog doesn't know is error 6. Pano Scrobbler's GNU FM account
  tags this way from its info screen.
- `.getTags`: the tags `user` (else the session's user) put on the
  entity, as visible as the profile (17 private, 6 leaving or blocking).
  Pano reads them back after tagging.
- `.getTopTags`: the tags shown on the entity, `count` its weighted
  taggers, past the minimum; `track.getInfo` fills `toptags` the same way.
- `user.getTopTags`: the tags a user uses, with how many entities; Pano
  fills its tag suggestions from it.
- `artist.getInfo`: the artist with its community bio (`summary` the first
  paragraph, `content` all of it, `published` the current revision's
  time), its five top tags, `playcount` past the minimum and, with
  `username` or a session, `userplaycount` where that profile may be seen;
  no `listeners` (not kept) or similar artists. pylast reads its bio.

Left out: `tag.getInfo`, `tag.getTopArtists` and the other tag pages,
and `album.getInfo`. Pano
Scrobbler sends `artist.getInfo`, `album.getInfo`, the `getTopTags`
methods and the tag pages to last.fm itself whatever server is set, so a
GNU FM account shows last.fm's tags and bios there; nothing seen in the
clients' sources calls the tag pages on a configurable server.

### Bios and tags measured

On a synthetic population (`population.sql` with 4,000 accounts and 3
days of plays, 199,464 scrobbles: tags and bios read accounts and the
catalog, not plays) with `community.sql`: 727,521 taggings on 50,072
entities (the most tagged artist has 30,442), 3,000 tags, one account at
49,999 taggings; 32,740 bio revisions (imports of 5,000 artists, a third
edited since, one artist with 10,001 revisions and a 10,000-character
text). `user_tags` takes 174 MB with its indexes (about 240 bytes a
tagging), `bio_revisions` 49 MB, `tag_counts` 72 MB for 181,585 rows
(20,414 shown past the minimum). TimescaleDB 2.29.2 / Postgres 18 in a
container (`shared_buffers=512MB`), the release API and Redis on the same
8-core desktop, one client at a time: oha for reads (200 requests on one
connection), curl for writes; p50 / p99.

| | p50 | p99 |
| --- | --- | --- |
| a bio, typical / of the artist with 10,001 revisions | 1.7 / 7.7 ms | 5.9 / 31.8 ms |
| a bio, signed in (blocks checked) | 3.0 ms | 9.2 ms |
| history, first page of 50 (10,001 revisions) / signed in | 4.4 / 5.9 ms | 11.5 / 11.4 ms |
| history, a page of 200, 9,000 deep | 10.5 ms | 17.4 ms |
| a revision with 10,000 characters | 1.3 ms | 5.5 ms |
| an entity's tags: the most tagged artist / a typical one / a track | 2.2 / 1.9 / 2.3 ms | 7.5 / 6.1 / 6.8 ms |
| a tag's artists: first 50 / 200 / a rare tag | 3.6 / 4.0 / 2.1 ms | 19.7 / 18.3 / 7.4 ms |
| my tags, 49,999 taggings: first 50 / 200 / 200 at 24,000 deep | 2.6 / 8.2 / 26.5 ms | 27.9 / 15.9 / 36.1 ms |
| my tags on one entity, 49,999 taggings / typical | 1.5 / 1.7 ms | 24.9 / 4.2 ms |
| a user's tags, 49,999 taggings / typical | 19.5 / 3.0 ms | 35.6 / 7.2 ms |
| `artist.getInfo` (most tagged, 10,001 revisions) / `artist.getTopTags` / `user.getTopTags` at 49,999 | 9.7 / 2.7 / 18.4 ms | 13.8 / 8.5 / 28.0 ms |
| a bio edit (40, each a new bio's revision, duplicate check included) | 8.8 ms | 12.8 ms |
| a revert on the 10,001-revision bio (20) | 11.4 ms | 13.5 ms |
| add 2 tags / remove one (100 each) | 7.6 / 4.6 ms | 15.0 / 10.5 ms |
| add a tag to the most tagged artist (the entity cap's count, 19) | 7.6 ms | 11.6 ms |
| a report (20) | 12.6 ms | 27.0 ms |

My tags on one entity first took 40.6 ms for the heaviest tagger: one
statement with an optional entity got a generic plan that read the whole
list; two statements fixed it. The worker recounts all 50,072 entities in
14–18 s (batches of 500), the most tagged artist alone in 0.73 s
(1.1 s while each tagger's factor was a lateral lookup instead of one
grouped scan of `coordination_days`). An entity's count is at most a
minute late after a change, or 6 hours for weights changing on their own.

The undo, measured: six aged accounts holding the current revision of 300
bios and a tag on 600 popular artists (each showing it, past the
minimum). `worker moderation suspend farm1 … farm6 --undo` put the 300
bios back and took the tag off all 600 in 4.4 s, 3.3 s of it the recount
of those artists; `block-tag spam` over the same 600 took 3.6 s.

Scrobbling is untouched: no trigger or function on `scrobbles` reads the
new tables, and ingest calls none of their queries (not measured).

```bash
psql "$SCRATCH_URL" -v users=4000 -v days=3 -f scripts/synthetic/population.sql
psql "$SCRATCH_URL" -f scripts/synthetic/community.sql
DATABASE_URL="$SCRATCH_URL" worker tags refresh
```

## Similar artists, taste neighbours and recommendations

What listening says about artists and people: an artist's similar artists
(whose listeners also play it), a user's taste neighbours (accounts that
chose to be shown, whose listening is closest), and artists recommended to
the signed-in user. The worker computes all three once a day from the
ranking weights and coordination factors and stores them; the routes read
what is stored and filter what changed since. How they are computed, and
what an attack on them costs: [rankings.md](rankings.md#similarity).

| route | access | |
| --- | --- | --- |
| `GET /v1/artist/{id}/similar` | public | a page; 404 no such artist |
| `GET /v1/user/{username}/similar` | public (a session sees its own) | the taste neighbours; 403 private, 404 missing, leaving, blocking the viewer, or not shared |
| `GET /v1/user/me/recommendations/artists` | `read` | a page |
| `POST /v1/user/me/recommendations/artists/{id}/skip` | `write` | 204; 404 no such artist |
| `DELETE /v1/user/me/recommendations/artists/{id}/skip` | `write` | 204, also when not skipped |
| `GET /v1/user/me/similarity` | `read` | the setting |
| `PUT /v1/user/me/similarity` | session | `{ "mode": … }`; 400 unknown mode |

None has a limit of its own beyond the global one (60 requests a minute
per address): a read is a few index lookups on stored rows, a skip one row
(the newest 1,000 kept), a change of setting at most one indexed delete,
and no request makes the worker compute anything.

### The setting

`users.similarity`, one of:

| mode | the account's listening | shown as a taste neighbour | its own recommendations and neighbours |
| --- | --- | --- | --- |
| `listed` | counts, anonymously, in similar artists and others' recommendations | yes, while the profile is public and the account isn't leaving | yes, and its neighbours are anyone's to see on its profile |
| `anonymous` (the default) | counts, anonymously | never | yes, its own to see |
| `off` | not used at all | never | none: nothing is computed from or for it |

Changing it takes effect at once for what names the account: leaving
`listed` deletes the account from every stored neighbour list, `off`
deletes its recommendations and neighbours too; a run computing meanwhile
stores none of them. Its anonymous part in similar artists goes at the
next daily run (a change doesn't force one: that would let anyone make the
server recompute everything as often as they liked). The default is the
private side for being shown and the useful side for counting, which is
anonymous and needs at least 10 accounts per pair
([privacy.md](privacy.md#similarity)).

`GET` answers `{ "mode": "anonymous", "shown": false, "changed_at": null }`;
`shown` is whether others see the account as a neighbour now (`listed` and
public). `PUT` takes a session, never an API token, even one that may
write: a leaked token mustn't put the account on display.

### Similar artists

```json
{
  "artist_id": 97,
  "computed_at": "2026-10-14T03:12:40Z",
  "stale": false,
  "total": 50, "offset": 0, "limit": 2,
  "entries": [
    { "position": 1, "artist_id": 12, "artist_name": "Portishead",
      "image_url": "https://…/artists/3f/….jpg", "score": 0.412, "common_listeners": 200 },
    { "position": 2, "artist_id": 31, "artist_name": "Tricky",
      "image_url": null, "score": 0.377, "common_listeners": 50 }
  ]
}
```

- `limit` 20 (at most 50), `offset` (negative: 400; past the end: an empty
  page with its `total`); 50 at most per artist.
- `score` 0 to 1, three decimals. `common_listeners` is a lower bound from
  1, 2, 5, 10, 20, 50, 100, … of the accounts listening to both, never an
  exact count: one account arriving or leaving can't be read off it.
- An artist with too few listeners has none (`total: 0`); before the first
  run `computed_at` is `null` (cacheable 60 s).
- `Cache-Control: public, max-age=3600, stale-while-revalidate=3600` and a
  weak `ETag` of the run; `If-None-Match` gets a 304 after one primary-key
  lookup. `stale` is true an hour past the next run's due time (the worker
  stopped or failing), and the last results are still served.

### Taste neighbours

```json
{
  "status": "ready",
  "computed_at": "2026-10-14T03:12:40Z",
  "stale": false,
  "entries": [
    { "user": { "id": 70, "username": "ana", "display_name": "Ana", "image_url": null },
      "score": 0.731, "shared_artists": 12,
      "top_shared": [ { "artist_id": 97, "artist_name": "Massive Attack" },
                      { "artist_id": 12, "artist_name": "Portishead" } ] }
  ]
}
```

- **Who appears**: accounts that chose `listed`, public, not waiting to be
  deleted, and not blocked by or blocking the user whose list it is, nor
  the signed-in viewer. Checked at every request, so a block, a private
  profile or a change of setting hides an account at once (a block also
  deletes the pair's stored rows).
- **Whose list**: the user's own, signed in, whatever their setting (`off`:
  `status: off`, no entries). Someone else's only when that user is
  `listed`, and then it is anyone's (it says whom they resemble, which
  they chose to show); otherwise 404 with the body a missing user gets.
  A private profile's list answers 403, an account blocking the viewer or
  waiting to be deleted 404, as for its profile.
- `limit` 10, at most 30; 30 are kept per user. `status`: `ready`, `none`
  (too little listening, or no listed account close enough), `off`,
  `pending` (nothing computed yet). `Cache-Control: private, max-age=60`,
  `Vary: Authorization`.
- `top_shared` names artists the neighbour listens to: they chose to be
  shown, and their charts say as much.

### Recommendations

```json
{
  "status": "ready",
  "computed_at": "2026-10-14T03:12:40Z",
  "stale": false,
  "total": 87, "offset": 0, "limit": 20,
  "entries": [
    { "position": 1, "artist_id": 31, "artist_name": "Tricky", "image_url": null,
      "score": 1.0,
      "because": { "artist_id": 97, "artist_name": "Massive Attack" },
      "because_count": 3 }
  ]
}
```

- Artists the user never played, ranked by how much the listeners of the
  artists they play also play them. `because` is one of the user's own
  artists, the one that counted most: "listeners of Massive Attack also
  play this"; `because_count` how many of the user's artists led to it.
  Nothing names another account.
- The user's whole history counts, imports included, recent listening
  more; what other accounts play counts as for similar artists.
- **Left out at once**: an artist the user played since the run (plays up
  to 14 days older than the run arriving late included) or skipped. A skip
  (`POST …/{id}/skip`) hides an artist for 90 days, any artist,
  recommended or not; `DELETE` takes it back; the newest 1,000 are kept.
  Skipped plays (the classifier's, or anything scrobbled) are plays: their
  artists are left out like any played.
- `limit` 20, at most 100, `offset`; 100 kept per user. `score` is
  relative to the first (1.0). `status` as for neighbours.
- A weak `ETag` of the run, the user's latest play and their skips, and
  `Cache-Control: private, no-cache`: a client revalidates every time and
  gets a 304 while nothing changed.

### Cold start

| | needs | otherwise |
| --- | --- | --- |
| an artist's list | 10 accounts' credit of listeners, and neighbours with 10 accounts and a tenth of its listeners in common | `total: 0` |
| recommendations | 5 distinct artists in the history, a play in the last 180 days | `status: none` |
| taste neighbours | the same, and a listed account sharing 3 rarity-weighted artists | `status: none` |
| a new account's listening in others' results | the weights: nothing before 7 days, 4 full plays for a full credit | |

All are `SIMILARITY_*` settings ([rankings.md](rankings.md#similarity)).
An account whose plays are all older than 180 days gets nothing until it
scrobbles again.

### Measured

On the population of [rankings.md](rankings.md#attacks) (5,521 accounts,
5.2M scrobbles), the release API on the same 4-core machine as the
database, one client on a keep-alive connection, 2,000 requests cycling
through 300 artists or users (p50 / p99):

| request | p50 | p99 |
| --- | --- | --- |
| similar artists, a page of 20 | 1.6 ms | 5.4 ms |
| the same, `If-None-Match` hit (304) | 0.56 ms | 0.94 ms |
| a listed user's taste neighbours, signed out | 3.1 ms | 5.5 ms |
| one's own taste neighbours, signed in | 3.5 ms | 6.5 ms |
| recommendations, a page of 20 | 7.0 ms | 15.5 ms |
| the same, `If-None-Match` hit (304) | 2.2 ms | 3.4 ms |
| recommendations, `limit=100` | 7.0 ms | 9.7 ms |
| recommendations of the three-year heavy listener | 5.7 ms | 10.2 ms |

A recommendations page filters all 100 stored entries against the user's
skips and their plays since the run, by (user, artist, time) on the
scrobbles index, which is most of its cost; nothing here needs a cache
beyond the ETags. The daily run that makes them: [rankings.md](rankings.md#cost-1).

## Access

| route | access |
| --- | --- |
| `GET /v1/user/{username}/charts/{artists,tracks,albums}` | public (a session sees its own private profile) |
| `GET /v1/user/{username}/followers`, `/following` | public (likewise) |
| `GET /v1/album/{id}` | public |
| `GET /v1/user/me/community/{artists,tracks,albums}` | `read` |
| `GET /v1/user/me/feed` | `read` |
| `GET /v1/user/{username}/loved` | public (likewise) |
| `GET /v1/user/me/loved` | `read` |
| `POST`, `DELETE /v1/track/{id}/love` | `write` |
| `GET /v1/track/{id}` | public (a session gets `loved`) |
| `POST`, `DELETE /v1/user/{username}/follow` | `write` |
| `POST`, `DELETE /v1/user/{username}/block` | session |
| `GET /v1/user/me/blocks` | session |
| `DELETE /v1/user/me/followers/{username}` | session |
| `GET /v1/artist/{id}/similar` | public |
| `GET /v1/user/{username}/similar` | public (a session sees its own) |
| `GET /v1/user/me/recommendations/artists` | `read` |
| `POST`, `DELETE /v1/user/me/recommendations/artists/{id}/skip` | `write` |
| `GET /v1/user/me/similarity` | `read` |
| `PUT /v1/user/me/similarity` | session |
| `GET /v1/{artist,album}/{id}/bio`, `/bio/history`, `GET /v1/bio/revisions/{id}` | public (a session hides blocked and blocking authors) |
| `PUT /v1/{artist,album}/{id}/bio`, `POST …/bio/revert` | `write` |
| `POST /v1/reports` | `write` |
| `GET /v1/{artist,album,track}/{id}/tags`, `GET /v1/tag/{tag}/{artists,albums,tracks}` | public |
| `POST /v1/{artist,album,track}/{id}/tags`, `DELETE …/tags/{tag}` | `write` |
| `GET /v1/user/me/tags` | `read` |
| `GET /v1/user/{username}/tags` | public (a session sees its own private profile) |

Public routes take an optional session (`optional_auth`); an API token,
even with `read`, is ignored there, as on every profile route. Search, the
listeners of an artist or a track and comments take one too, to leave
blocked and blocking accounts out. Blocks take a session: a leaked token
mustn't show whom the user protects themselves from, nor lift a block (the
apps' interfaces use the session anyway); so does changing the similarity
setting, which can put the account on display. `router/tests.rs` lists each.

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

- **Loves as a lever**: none. Loving takes no listening, so loves steer
  nothing (no ranking, recommendation, similarity or album decision reads
  them) and no count of them is published; a farm loving a track changes
  only its own accounts' lists. The love limits bound how many catalog
  entries the Last.fm-compatible API can create by loving names.
- **Appearing in someone's taste neighbours**: only by choosing
  `listed`, and only while public; an account imitating someone's
  listening to show up beside them needs a week of weighted plays like
  anyone, and is gone from their list the moment it is blocked.
- **Steering similar artists or recommendations**: the anti-botting
  weights and coordination factors apply, and a farm needs a tenth of an
  artist's listeners to reach its list ([rankings.md](rankings.md#attacks)).
- **Harassment**: block the account (it no longer finds you signed in,
  and can't follow you), or remove it from your followers (30 days without
  following you again). A follow farm can be removed or blocked at 200
  accounts an hour.

- **A vandal** needs an account 7 days old; then each edit is visible at
  once (there is no review but the spam heuristics), 30 an hour, 50 bios
  a day, 3 revisions of one bio a day. Anyone eligible reverts it; one
  `worker moderation suspend NAME --undo` puts back every bio showing its
  text and stops it.
- **A farm of aged accounts** writes 3 revisions of a bio a day per
  account, outlasting honest reverters only by numbers; with 5 accounts it
  puts a tag on any entity's visible list, 120 entities an hour per
  account (300 tag changes an hour per address, so 60 entities an hour
  from one address with 5 accounts). Its links across bios are held from
  the third bio per account. The operator's undo doesn't grow with what the
  farm did: one `suspend NAME... --undo` naming every account puts back
  every bio and takes their weight out of every tag count in one command
  ([measured](#bios-and-tags-measured): 4.4 seconds for six accounts, 300
  bios and 600 tagged artists), and `block-tag` takes a spam term off
  everything.
- **What an attacker needs** to stay ahead: accounts 7 days old (cheap to
  age in bulk: registration isn't limited beyond the per-address request
  rate), several addresses to pass the per-address limits, and listening
  that doesn't trip the coordination detection, which only judges
  listening: accounts that edit and tag but don't scrobble are never
  flagged, so they keep full weight until someone reports them. Then it is
  their rate against the operator's attention: nothing detects an edit or
  tag farm on its own.
- **Probing the counts:** a tag's weighted count says how much a farm's
  accounts weigh, as the rankings do; it needs five of them to read any
  number.

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
albums 1.1 to 1.3 times (each row of `ranking_daily` joined to its
track's album once; [Albums](#albums)). The history is 60 days long, so 90 days reads 60 and 12 months
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
  for the one its accounts named ([Albums](#albums)), so a deluxe
  edition's chart holds only its bonus tracks. Community album charts map
  every (account, day, track) to its album by joining `tracks`, which
  grows with the catalog.
- **First views of long histories:** a cold all-time album chart of an
  account with 150,000 plays takes ~300 ms, more with more; only repeat
  views are cached.
- **Feed reach:** 14 days, and a feed doesn't merge or collapse repeats
  of one account.
- **No follow approval, no muting, no notifications.**
- **Loves are tracks only** (no loved artists or albums), are never
  counted per track, and steer nothing.
- **Bios** are artists' and albums' only, plain text, published without
  review unless a spam heuristic holds them; there is no diff route
  (clients compare two revisions' texts), no watchlist or notification
  of changes, and no moderator role but the operator's commands.
- **Tags** show only past the minimum of weighted accounts, are not
  imported from Last.fm ([Tags](#tags)), and steer nothing.
- **Similarity is a day old at most**, plays and skips since aside; a new
  account's listening counts for others after a week (the weights), a
  user with fewer than 5 artists gets no recommendations, and an artist
  with fewer than 10 listeners no neighbours. There are no similar
  tracks ([rankings.md](rankings.md#cost-1) says why), no explanation
  beyond one artist per recommendation, and no "not interested in this
  artist's neighbours" beyond skipping each.
- **Taste neighbours** are found among listed accounts only; with few
  listed accounts, lists are short or empty (`status: none`).
