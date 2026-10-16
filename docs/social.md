# Social features: internals

How follows, blocks, charts, the feed, albums, loved tracks, bios and tags
work inside, what they cost and what they defend against. What clients see
(routes, parameters, answers) is in the API guides, `docs/api/`, which the
docs page shows; similarity and the rankings' weights are in
[rankings.md](rankings.md), what is stored and erased about an account in
[privacy.md](privacy.md).

## Who sees whom

One rule everywhere an account's data or name is shown: an account is
visible to a viewer when it isn't waiting to be erased (`delete_after`)
and it is public, or it is the viewer's own.

- **A profile and everything under it** go through
  `middleware/visibility.rs::ensure_profile_visible`: `404` for an account
  waiting to be erased or blocking the viewer (checked before privacy, so a
  blocked viewer can't tell a private profile from a missing one), `403`
  for someone else's private profile. The scrobbler APIs use the same
  answers (`compat::lastfm::visible_user`, `seen_user`).
- **A list naming people** filters in SQL: `NOT u.is_private` (but the
  viewer's own entry), `u.delete_after IS NULL`, and for a signed-in viewer
  the block filter both ways:
  `NOT EXISTS (SELECT 1 FROM user_blocks b WHERE (b.blocker_id = u.id AND b.blocked_id = $viewer) OR (b.blocker_id = $viewer AND b.blocked_id = u.id))`.
  A new list naming people needs all three.
- **Authors** of comments, image candidates and bio revisions are named
  whatever their privacy, without the private profile's data
  (`UserSummary`, not `UserProfile`).
- **Aggregates** count private accounts unnamed, behind
  `RANKING_MIN_LISTENERS` where a number could stand for a few people
  (catalog counts, rankings, tag counts, community charts).

## Follows and blocks

- **Follows** (`user_follows`): pages by a keyset `Cursor`
  (`shared::cursor`) over indexes on both directions (migration `0028`).
  `MAX_FOLLOWING` (2,000) is checked under a per-follower advisory lock.
  Counts on profiles are every follow, read from the index.
- **Blocks** (`user_blocks`, migration `0031`) end the follows both ways
  in the same transaction; follows, blocks and removals take the pair's
  advisory lock (`blocks::lock_pair`), so no follow lands beside a block
  made at the same moment. A block publishes `close:{viewer}` on the
  blocker's now-playing channel, which the live hub obeys and never
  forwards (`live::close_for`); a profile turning private publishes
  `close:others`, which every stream but the owner's obeys.
- **Removed followers** (`follower_removals`) can't follow again for 30
  days; the worker's cleanup deletes expired rows.
- **Limits** (`limits.rs`, Redis, fail open): follows 100/h per account and
  300/h per address; blocks, unblocks and removals 200/h and 600/h; 3 new
  follows and 3 blocks of one account by another a day (`pair_attempt`),
  so following and unfollowing can't keep putting an account in front of
  another; 10,000 blocks per account. Unfollowing is free.

## A user's charts

`db::queries::charts::user_{artists,tracks,albums}` read the daily
aggregates (`scrobbles_daily_by_artist`, `_by_track`) and subtract the
classifier's duplicates from `scrobble_flags`, never the scrobbles
hypertable. A page is cut (`LIMIT`/`OFFSET` over the counted rows) before
names and images are joined. The album chart sums the user's daily rows
by track, then maps tracks to `tracks.album_id`: joining every daily row
to `tracks` instead took 2.3 times as long (2.7 s against 1.2 s for three
years of plays in one psql run).

The handler (`handlers/charts.rs::personal`) checks visibility, then a weak
ETag of the user's `scrobble_breakdown` and the hour, then Redis
(`chart:v1:{user}:{kind}:{period}:{from}:{to}:{limit}:{offset}:{etag}`,
15 minutes): every viewer allowed to see the profile shares the page, and
a new play changes the breakdown, hence the key.

## Community charts

`db::queries::charts::community_*` rank the visible followed accounts'
`ranking_daily` weights and coordination factors as `rankings()` does
([rankings.md](rankings.md)), entries under `RANKING_MIN_LISTENERS`
credits left out.

- **Budget**: at most `shared::charts::COMMUNITY_USER_DAYS` (14,600)
  (account, day) pairs, the most recently active accounts first
  (`users.last_seen_at`): every follow (2,000) over a week, 40 over a
  year. The query is linear in those pairs, about 20 ms a thousand, so a
  chart costs the same whatever the follows.
- **Plans**: the transaction sets `plan_cache_mode = force_custom_plan`
  (after five runs Postgres switched the prepared statement to a generic
  plan three times as slow) and `enable_mergejoin = off`.
- **Cache**: the computed ranking (1,000 positions at most) is kept 5
  minutes in Redis under `community:v1:` and a hash of the kind, the days,
  the minimum and the sorted account ids. A follow, an unfollow or a
  followed account turning private or leaving changes the key, so nothing
  is invalidated and no viewer gets a ranking of accounts it doesn't
  follow. Redis failing costs only the computation.
- **Probing**: entries need `RANKING_MIN_LISTENERS` credited accounts, so
  a chart of one or two follows can't show one account's weights.

## Albums

Each track counts for one album (`tracks.album_id`) in charts, community
charts and `Album.scrobble_count`. Pure rule in `shared::albums`, votes and
queue in `db::queries::albums` (migration `0032`), the `album attribution`
loop and `worker albums backfill` in `crates/worker/src/albums.rs`.

- **Votes** (`track_album_votes`): one per account and track, the album
  its plays named, the latest when it changes; recorded after ingest's
  commit (`record_vote`, one primary-key probe, a write only on a change)
  and per import page.
- **Weight**: one per account at least `RANKING_MIN_ACCOUNT_DAYS` old and
  not waiting to be erased, times its lowest coordination factor of the
  last 28 days; doubled for a release title MusicBrainz lists the
  recording on (`tracks.mb_release_titles`, kept only for the track's own
  mbid). A new album takes the track with 1.5 times the current album's
  score and one account more, so a close race doesn't flip it.
- **Queued** (30 s settle, 200 tracks a transaction) on a vote change, at
  an account's coming of age (`recheck_at`), at a deletion request,
  restore and erase, when release titles change, at
  `tracks mb-review --apply`, and for the accounts whose factor a
  coordination run changes or `clear_factors` removes.
- **Not stored per play**: an album column in `ranking_daily` measured
  faster for community album charts but would need rewriting rows
  whenever a track moves, and 8 bytes a row more; charts join `tracks`
  once per (account, day, track) row instead.
- **What moving a track takes**: half again as many established,
  unflagged accounts as stand behind its album, each having played it from
  the other album last (`scripts/synthetic/album-attack.sql`: 99 accounts
  behind an album lose it to 149, 98 keep it against 146; one account
  playing a track 1,000 times moves nothing).
- **Backfill**: `worker albums backfill [--dry-run]` reads every stored
  scrobble's album, a chunk a statement, into a temporary table, then
  decides every track; once after migrating to `0032`.

## Feed

`db::queries::feed` reads the visible followed accounts' scrobbles of the
last 14 days (`FEED_DAYS`) by keyset (`played_at`, `id`), suspect plays
and duplicates left out. A page walks back from its cursor in windows of
an hour, four times longer each, until full or 14 days back; each window
reads by (account, time) on `idx_scrobbles_user_time` only the accounts
whose `last_seen_at` reaches it, which therefore must be the latest play
(the counter trigger keeps the greater since migration `0030`). Chunks
older than 30 days are compressed, beyond the feed's reach.

## Loved tracks

`loved_tracks` (migration `0034`): one row per (user, track) with
`loved_at`, `source` and the client or import. Queries in
`db::queries::loves`, routes in `handlers/loves.rs`, protocols in
`compat/` (`love_by_name`, ListenBrainz feedback).

- **Loves feed nothing**: no ranking, weight, similarity, recommendation,
  album decision or classifier reads them, and no count of a track's loves
  is served. A love takes no listening, so it would be the cheapest number
  to inflate; giving loves a say anywhere first needs anti-botting of its
  own.
- **The cap** (`MAX_LOVED_TRACKS`, 50,000) is counted under a per-account
  advisory lock (`'loves:' || user_id`), which the import takes too.
- **Flags**: the viewer's on `TrackWithCredits`, the player's on recent
  scrobbles and feed entries, joined on the primary key (an `EXISTS` there
  was planned as a hashed scan of the whole table).
- **Catalog**: native loves are by id and create nothing; Last.fm loves by
  name resolve the track as scrobbling does (`tracks_db::resolve`); unloves
  never create. Loves go with their track and user (`ON DELETE CASCADE`).
- **Imports**: loves come first, 1,000 a page under the import's lease;
  a re-import reads only loves newer than its window, so an unlove sticks.

## Bios

Pure rules in `shared::community` (text, links, `Editor::refusal`),
storage in `db::queries::bios` (migration `0037`), routes in
`handlers/bios.rs`, operator commands in `worker moderation`.

- **Revisions** (`bio_revisions`) are kept for good; `entity_bios` holds
  the current one and a lock, and `artists.bio` mirrors its text
  (`bios::set_current` is the only writer).
- **Who may edit** (`bios::editor`): an account `RANKING_MIN_ACCOUNT_DAYS`
  old, not suspended, not waiting to be erased, whose lowest coordination
  factor over 28 days is at least 0.5 (`MIN_EDITOR_FACTOR`). The refusal
  says only "can't edit now" for the last three, though an account that
  can still tag and has no suspension in its export can infer a flag.
- **Caps**, counted under the `entity_bios` row lock: 50 revisions in 24
  hours per author, 3 of one bio. Hourly limits: 30 per account, 60 per
  address (Redis, fail open).
- **Held for review** (`202`): a link host repeated more than twice
  (`MAX_SAME_HOST`); a host its author added to 2 other bios in 7 days, or
  anyone to 4 in 24 hours (`added_hosts`, GIN-indexed); a text of 200
  characters or more equal to another bio's revision (`body_hash`,
  whitespace and case folded). Reverts are never held.
- **Imports from Last.fm** become `import` revisions (`bios::import`):
  when the artist has none, over an earlier import on a forced refresh,
  never over a person's edit, a lock or a hidden revision.

## Tags

Names in `shared::community::tag_name`, storage and counts in
`db::queries::tags` (migration `0038`), routes in `handlers/tags.rs`, the
`tag counts` loop and `worker tags` in `crates/worker/src/tags.rs`.

- **Caps**: 10 per account per entity, 500 distinct per entity, 50,000
  per account, counted under per-account then per-entity advisory locks;
  120 changes an hour per account and 300 per address.
- **Counts** (`tag_counts`): per (entity, tag), every tagger and their
  weight in thousandths: `FULL` from `RANKING_MIN_ACCOUNT_DAYS` after
  registering, at the account's lowest coordination factor of 28 days,
  nothing while suspended or leaving. A tag shows from
  `RANKING_MIN_LISTENERS` × `FULL`. The loop recounts an entity 30 s after
  a change (`tag_queue`) and every entity every 6 hours, since weights
  change on their own; deletion requests, restores, erases and
  suspensions queue the account's entities (`tags::enqueue_user`).
- **A tag below the minimum reads as unused**: the tag page answers an
  empty page, and reports take only tags visible somewhere
  (`tags::visible_tag_id`).
- **The blocklist** (`tag_blocklist`, `worker moderation block-tag`)
  refuses tags holding a term's words in a row and takes them off
  everything.
- **Not imported** from Last.fm: no call lists a user's taggings
  (`user.getPersonalTags` needs a call per tag and kind), so an import
  would take thousands of calls and land past the hourly limits.

## Abuse

- **Follow farms** fill a follower list and count, slowed by the cap and
  the rates; following opens nothing a private profile hides, and reaches
  no one's feed or community chart, which show only accounts the viewer
  follows.
- **Flooding a feed**: a followed account scrobbles up to the daily
  allowance; the classifier's suspect plays and duplicates are left out.
- **Probing the weights** through community charts or tag counts needs
  `RANKING_MIN_LISTENERS` aged accounts on one entity, and then reads their
  sum, what the global rankings show anyway.
- **Cost**: a community chart is bounded by its user-days, a feed page by
  the plays in its windows; each custom range is a new cache key, so a
  client can have one computed per request, within the per-address limit.
- **A vandal** needs an aged account; each edit is visible at once unless
  held, and anyone eligible reverts it. One
  `worker moderation suspend NAME... --undo` puts back every bio showing a
  suspended account's text and takes its weight out of every tag count
  (measured: six accounts, 300 bios and 600 tagged artists in 4.4 s).
- **Edit and tag farms that don't scrobble** are never flagged by the
  coordination detection, which judges listening: they keep full weight
  until someone reports them.
- **Image votes** count every account equally and promotion is permanent
  (`MIN_VOTES_TO_DEFAULT`, 3): a handful of fresh accounts can set any
  artist's or album's image. Taking it down is an operator procedure
  ([uploads.md](uploads.md)).

## Costs

Measured on 2026-10-16: `scripts/synthetic/` with 4,000 accounts over 60
days (`population.sql` with 20 genres, `follows.sql`, `heavy.sql`,
`loves.sql`, `community.sql`: 4.1M scrobbles, 314,903 follows, 310,414
loves, 727,521 taggings, 32,331 bio revisions), classified, weighed, 152
of 158 chunks compressed; TimescaleDB 2.29.2 / Postgres 18 in a container
(`shared_buffers=512MB`), the release API and Redis on the same 8-core
desktop, one client at a time. "Warm": 200 requests on one connection,
p50 / p99. "Cold": Redis emptied before each request, p50 / max.

| route | p50 | p99 / max |
| --- | --- | --- |
| profile | 1.7 ms | 12 ms |
| followers page of 200 | 3.7 ms | 9.6 ms |
| recent scrobbles, typical account / three years of plays | 10.5 / 13.7 ms | 88 / 20 ms |
| feed, 100 / 2,000 follows | 9.5 / 14.5 ms | 28 / 21 ms |
| a user's chart, typical account, 7 days / all, cold | 5.0 / 26.9 ms | 41 / 38 ms |
| a user's album chart, three years of plays, all, cold | 258 ms | 273 ms |
| the same, cached | 1.7 ms | 3.0 ms |
| community tracks, 7 days, 100 / 2,000 follows, cold | 20.4 / 225 ms | 22 / 682 ms |
| community albums, 30 days, 2,000 follows, cold | 297 ms | 315 ms |
| community chart, cached | 8.0 ms | 15.7 ms |
| loved tracks, 49,999 loves, page of 200 | 11.5 ms | 16.0 ms |
| a bio (10,001 revisions) / its history | 4.8 / 3.4 ms | 9.3 / 5.9 ms |
| an entity's tags / a tag's artists | 1.5 / 2.5 ms | 5.0 / 16 ms |
| my tags, 49,999 taggings, page of 200 | 6.5 ms | 33 ms |
| love / unlove (typical account) | 4.0 / 2.4 ms | 10.9 / 3.9 ms |
| love at 49,999 loves | 11.3 ms | 22.6 ms |
| follow / unfollow | 6.0 / 3.4 ms | 14.6 / 6.8 ms |
| add 2 tags / remove one | 8.3 / 3.3 ms | 26 / 6.5 ms |

The worker recounts every tag (50,072 entities) in 12.5 s.

## Limits

- A year's community chart covers the 40 most recently active followed
  accounts; the response says how many it covered of how many it could.
- A track on several releases counts for the album its accounts named,
  so a deluxe edition's chart holds only its bonus tracks.
- A cold all-time album chart of an account with 150,000 plays takes a
  quarter of a second, more with more; repeat views are cached.
- No follow approval, muting or notifications; a blocked person can make a
  new account or read a public profile signed out; a private account
  following someone isn't in their follower list (lists never name private
  accounts), so it can be removed or blocked only by its username.
- Bios are artists' and albums' only, with no diff route or watchlist and
  no moderator role but the operator's commands.
- The rate limits fail open without Redis (uploads, exports, deletion
  requests and logins fail closed).
