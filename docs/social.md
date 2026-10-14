# Social: follows, charts and the feed

What users see of each other: follows, a user's own charts, the charts of
the accounts a user follows (community charts), a feed of what those
accounts play, and what listening says about artists and people (similar
artists, taste neighbours, recommendations). Global rankings are in
[rankings.md](rankings.md); what is stored and erased about an account, in
[privacy.md](privacy.md).

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
  and accounts waiting to be deleted (404 on their own routes). Taste
  neighbours name only accounts that chose to be shown, while public
  ([below](#taste-neighbours)); similar artists and recommendations name
  nobody.
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
- **Similarity is a day old at most**, plays and skips since aside; a new
  account's listening counts for others after a week (the weights), a
  user with fewer than 5 artists gets no recommendations, and an artist
  with fewer than 10 listeners no neighbours. There are no similar
  tracks ([rankings.md](rankings.md#cost-1) says why), no explanation
  beyond one artist per recommendation, and no "not interested in this
  artist's neighbours" beyond skipping each.
- **Taste neighbours** are found among listed accounts only; with few
  listed accounts, lists are short or empty (`status: none`).
